# super agent 原型手法索引

> 性质：**附录 / 索引**。核心与分层见 [`design.md`](./design.md)。
> 本文只回答「原型里有哪些**脱离了域也成立**的手法」，不做差距对照、不提落地建议 ——
> 那些判断在 `design.md`，本文不重复。
>
> 来源：`tmp/dsh-superagent-prototype`（未纳入版本控制）的 `docs/000`–`docs/008` 与 `src/`。
> 按 §7 的引用纪律只引小节名，不引 `path:line`。

---

## 1. 机制类（L0–L2，可直接对照本仓）

| 手法 | 原型里的形态 | 与本仓的关系 |
|---|---|---|
| 状态从事实推导，不存状态机 | `ticketStage` 是一张 first-match-wins 的表，每行由一个已存事实判定（`007-architecture.md` § Engine） | 本仓已有同构取向（ADR-0054 的四态机可改为此形态）。见 `design.md` §2.2 |
| 入站分三段可判事实 | 已送达 / 已读（取进历史且随后有报告或回合结束）/ 已处理（`007-architecture.md` § Engine） | 原语已有，归属与寿命缺。见 `design.md` §2.2 |
| 未处理的消息退回上游 | 工作结束时未读且未被「确认/回答/退回」的消息退回；已读无动作的不退（`003-execution.md` § Follow-ups and context Threads） | 本仓无对应动作 |
| 槽满 ≠ 失败 | `Effects` 的 `Later`：推迟但不计失败（`007-architecture.md` § Effects） | 做并发闸时的必要区分。见 `design.md` §6 |
| 幂等锚 | 「同一个事件被接受两次只产生一个工作」（`001-domain.md` § Concepts） | 与「可选重复，绝不丢失」互补；去重键需显式定义 |
| 顺序保证 | 同一工作的消息到同一会话按入队顺序；别的消息不排队等它（`003-execution.md` § Channel failure） | 局部有序而非全局有序 —— 值得借用 |
| 恢复备注 | 重启后要求先核目录 / git 历史 / 远端，再重做任何**外部**动作（`003-execution.md` § Restart） | 本仓有 `--resume` 的机械修复（ADR-0043），无此提示 |

## 2. 工程规约类（与域无关，可独立借用）

| 手法 | 原型里的形态 | 备注 |
|---|---|---|
| 健康板 | 每部分独占一个 entry，只能由它自己下一次成功清除，连续失败 3 次才报错（`007-architecture.md` § Store / Effects） | 本仓无对应机制 |
| 参数级硬拒绝 | `noWait` 守卫拦 `sleep`、>30s 超时、>5s job wait（`007-architecture.md` § Presets and wait limits） | 本仓同类规则目前**纯提示词**；有确定判据的应下沉为参数校验 |
| 覆盖必须记基线 | 每个 override 存 `base`；默认改动后标 **Default updated** 并给差量（`004-prompts.md` § Built-in parts and templates） | 可配提示词的现成答案：不记基线 ⇒ 团队文本在升级后停在旧语义 |
| 文档即测试 | 编号文档只能引用更早的；模块表与 `src` 逐文件一致；链接必须可解析（`tests/docs.spec.ts`） | 本仓已出现指针失效；一条「文档列出的路径必须存在」即可挡住 |
| 每个类一个文档块 | 每个 class 前必须有 JSDoc（`tests/class-docs.spec.ts`） | 机械可查，成本低 |
| 「被否决的想法」表 | 把否决理由当一等产物记录（`001-domain.md` § Rejected concepts） | 这个**格式**比内容更值得借 |

## 3. 反面参照（它做对而我们不该学的原因）

| 它的部分 | 为什么不能搬 |
|---|---|
| 授权体系（Policy / Mandate / 准入表 / accept mode） | 撞本仓硬约束「不做安全」。**不是难，是义务** —— 长出授权等于对多租户安全负责 |
| 域级两阶段判定 | 判断归上层 |
| 常驻服务、SQLite 持久域、outbox 回执、schema 迁移 | 我们是库 + 单进程 CLI |
| 面板 / i18n / glossary | 上层产品形态 |
| 子 agent 嵌套（深度 >1） | 会让「谁占槽位」变成二维；深度恒 1 是选择 |
| 44 个测试文件里的大半 | 测的是域规则的 case（谁能开工、何时恢复、策略变了怎么办）。**这套 case 才是真贵的**，不做域就不用背 |

## 4. 一句话

原型的 README 自己写明：research code、由 coding agent 大量代写、与论文不一定一致
⇒ 值钱的是**文档里的决策与反例**，不是 15k 行 TypeScript。
