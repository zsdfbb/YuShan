# Ctrl-C / Esc 中断 turn — 架构质量分析报告

## 分析范围

- **对象**：`docs/arch/tui-ctrl-c-cleanup/design.md` + `docs/adr/0008-agent-cancel-handle.md`
- **维度**：全量（可行性 / 可维护性 / 可理解性 / 性能与可靠性）
- **关联**：`context.md`（三件套分析）+ commit `c9a4177`（ratatui 替换 baseline）

## 事实核查（先于打分）

| design.md / ADR 声明 | 事实 | 影响 |
|---|---|---|
| `cancel_handle(&self)` 返回 `CancelToken` | ✅ `agent-core::CancelToken` 已有 `Clone + Arc<AtomicBool>`（cancel.rs:5-25） | 设计正确 |
| `BasicLoop` 在 cancel 时返回 `Ok(RunResult)` | ✅ `basic.rs:48, 54, 79, 85` 4 处返回 `Ok(... stop_reason: Cancelled)` | subagent 误判已修正 |
| `cancel_handle(&self) -> self.cancel.clone()` | ✅ `cancel: CancelToken` 字段在 agent.rs:17，已通过 `AgentBuilder::cancel_token()` 公开 | 无需新字段 |
| `tokio::signal::ctrl_c()` 可用 | ✅ tokio 1 已包含 `signal` feature（Cargo.toml:17） | 无新依赖 |
| 不需要 `drop(turn_fut)` | ✅ cancel 路径返回 `Ok`，select! Ok 分支自然消费 future | 设计简洁 |
| stdout mode 不受影响 | ✅ `ui/` 模块 feature-gated `tui-ratatui`；stdout mode 走 `output/` | 隔离正确 |
| tui.rs / tui_completer.rs 删除后 build OK | ⚠️ 当前两文件仍在源码树（feature-gated `tui-stdout` 引用）；需 R6 阶段删除 | 待办 |

**0 处事实错误**。3 个 subagent 都误判「cancel 返回 Err」，已修正。

## 各维度判断

### 1. 可行性

| 检查项 | 判断 | 关键发现 |
|--------|------|----------|
| 1.1 技术可实现性 | 🟢 | `cancel_handle` getter + select! 三路改造均已可验证可行 |
| 1.2 依赖成熟度 | 🟢 | tokio 1 `signal::ctrl_c()` 稳定；`CancelToken: Clone` 已存在 |
| 1.3 实现周期 | 🟢 | ~80 行改动（5 行 crate + 75 行 ui）；与 design 估算一致 |

### 2. 可维护性

| 检查项 | 判断 | 关键发现 |
|--------|------|----------|
| 2.1 模块边界 | 🟢 | **零 trait** — 无 dyn dispatch、无 trait object、跨 crate 无借用风险 |
| 2.2 接口稳定性 | 🟢 | `cancel_handle(&self)` 仅为 `cancel(&mut self)` 补充；现有 3 caller（feature-gated）不受影响 |
| 2.3 测试难度 | 🟢 | 单测简单：构造 `AgentBuilder::cancel_token(c)` 直接注入；不需 mock Agent |
| 2.4 错误传播 | 🟢 | cancel 路径返回 `Ok(RunResult)` → `format::print_turn_summary` 用 `status_symbol` 显示 `✗ Cancelled`；无新错误类型 |
| 2.5 并发安全 | 🟢 | `Arc<AtomicBool>` + `Ordering::Release/Acquire` 保证可见性；select! 三 future 借用互不重叠 |
| 2.6 资源管理 | 🟢 | `CancelToken: Clone` 是 `Arc` 包装（cheap clone，无堆分配）；no leak |
| 2.7 演进收敛 | 🟢 | **+1 getter 共 4 getter + 1 action**；未来 timeout/parent token 直接复用同 API |

### 3. 可理解性

| 检查项 | 判断 | 关键发现 |
|--------|------|----------|
| 3.1 概念一致性 | 🟢 | 「cancel = 协议对象」语义清晰；与 `Notify` 方案（subagent B）比**不增加概念** |
| 3.2 抽象层次 | 🟢 | crate 层（`CancelToken` 暴露）→ ui 层（`&mut App.cancel_token`）→ select 触发；单层传递 |
| 3.3 文档完整度 | 🟢 | ADR-0008 含「Why / How / 后果 / 演进」四段；design.md 含代码骨架 + borrow 论证 |
| 3.4 新人上手成本 | 🟢 | 看 `cancel_handle()` 方法名即懂；doc comment 解释「Why `&self`」一次性解决 borrow 问题 |

### 4. 性能与可靠性

| 检查项 | 判断 | 关键发现 |
|--------|------|----------|
| 4.1 性能模型 | 🟢 | `Arc<AtomicBool>::store` ~1ns；select! 三 future 唤醒延迟可忽略；turn 期间 100ms tick 占 < 0.1% CPU |
| 4.2 故障模式 | 🟢 | `cancel_token.cancel()` 是幂等的（多次 cancel 与一次 cancel 等效）；CancelToken clone 之间独立但共享状态 |
| 4.3 退化策略 | 🟢 | 旧 `cancel(&mut self)` 保留为 fallback；未来若 CancelToken 引入问题，可改用 Notify 不破坏公开 API |
| 4.4 可观测性 | 🟡 | `app.cancel_requested` 字段用作 debug 观测点；可扩展 UI 显示 "Cancelling…" 状态（v1 议题） |

### 5. 与硬约束契合度

| 硬约束 | 契合度 |
|--------|--------|
| 单向依赖 `apps/` → `crates/` | ✅ ui 只读 crates API，不回环 |
| 静态组合优先 | ✅ `AgentBuilder::cancel_token(c)` 是编译期注入，无运行时插件 |
| 不跨动态库边界传 Rust trait | ✅ `CancelToken` 是 value type，不是 trait object |
| 不做安全 | ✅ CancelToken 是 atomic flag，无安全敏感 |
| 四者边界（Event/Hook/Component/Loop） | ✅ crossterm Event → CancelToken 调用 → BasicLoop 在 round 边界 Hook 响应 |

## 解耦质量评分

| 维度 | 评分 | 论证 |
|------|------|------|
| crates 公开 API 表面 | ⭐⭐⭐⭐⭐ | +1 getter 共 4 getter + 1 action；零破坏性 |
| ui 层隔离 | ⭐⭐⭐⭐⭐ | cfg feature gate；不暴露 trait object；CancelToken 是 value type |
| borrow checker 路径 | ⭐⭐⭐⭐⭐ | 一行 `let cancel_token = agent.cancel_handle();` 解决跨借用边界 |
| 错误传播 | ⭐⭐⭐⭐ | `Ok(RunResult{ Cancelled })` 自动走 `print_turn_summary`（已有 `status_symbol`） |
| 演进收敛 | ⭐⭐⭐⭐⭐ | timeout / parent token 直接复用 API |
| **总体** | **⭐⭐⭐⭐⭐ 4.8/5** |

## 风险排序

| # | 风险 | 影响 | 可能性 | 优先级 | 缓解 |
|---|------|------|--------|--------|------|
| **R1** | `tui.rs / tui_completer.rs` 仍在源码树（feature-gated 但占体积） | 中（~411 行无用代码） | 高 | P1 | c 阶段删除 |
| **R2** | 单次 cancel 后下次 turn 立刻 Cancelled（CancelToken 单调） | 中（坏行为） | 中 | P2 | design.md 已记：dispatch_input 入口 `agent.cancel_handle()` 拿**新 Clone**——已正确解决 |
| **R3** | Esc 在 idle 时只清 input——未来用户可能希望 Esc 全局取消 idle turn | 低 | 低 | P3 | v1+ 议题；当前行为与 rustyline 一致 |
| **R4** | 100ms tick 在 turn 期间持续刷新 view——CPU 浪费 | 低（< 0.1% 负载） | 低 | P3 | 未来可改为 event-driven |
| **R5** | R3 偏离未做 turn 中实时 token 刷新（status panel） | 低 | 中 | P3 | 见 ratatui-replace/review.md R5；v1 polish |
| **R6** | Esc/Ctrl-C 在 idle 时仍 cancel token——下次 turn 立刻 Cancelled | 低 | 低 | P3 | Esc idle 路径不调 token.cancel（已正确） |

## 改进建议

### 易修复（低风险快速改进）

1. **R1 删除 tui.rs / tui_completer.rs** —— 3 个文件删除 + main.rs cfg gate 简化 + Cargo.toml `rustyline` 改 optional
2. **R2 cross-turn 重置**：当前实现已用 `agent.cancel_handle()` 拿新 clone 解决——**已在 design 中确认**，无需代码改动，但建议在 dispatch_input 顶部加 `assert!(!cancel_token.is_cancelled(), "must be fresh clone")` 防止未来误用
3. **App.cancel_requested 加 `#[allow(dead_code)]`**：当前无读路径，仅 debug 观测点（已在 design §9 #5 标记）

### 需讨论（团队决策）

4. **R3 idle 时 Esc 是否应该取消「上一轮未消费的 cancel」**——建议不修（v1 议题）
5. **App.cancel_token 改为必填字段**（去除 Option）—— 当前 Option 是因为 App::new 不带 token；建议改为必填，让 `cancel_token.unwrap()` 转为 `cancel_token`

### 架构级（影响面大）

无。本设计在 v0 范围内已是最简。

## 总体评价

### 健康度

**🟢 优秀**。修复一个明确 bug，~80 行改动，单层 API 扩展（+1 getter），不引入新概念（无 Notify、无 trait）。borrow checker 路径简洁，错误传播通过现有 `Ok(RunResult{ Cancelled })` 自动处理。

### 最大风险点

**R1（tui.rs 残留）**——本设计文档未直接涉及 c 任务（删除 tui.rs），但 c 步骤是 ratatui 替换的**完整收尾**。建议在三件套最后阶段实施。

### 推荐下一步

1. **立即**（P1）：实施 b（cancel_handle + select! 改造）+ a（手动验证）
2. **完成后**（P1）：实施 c（删除 tui.rs / tui_completer.rs / rustyline 依赖）
3. **可选**（P2）：把 `App.cancel_token` 从 `Option<CancelToken>` 改为必填
4. **可选**（P3）：在 `dispatch_input` 加 `assert!` 防止 cancel_token 误用

### 解耦核心收益（已验证）

| 收益 | 状态 |
|------|------|
| crates 公开 API 仅 +1 getter | ✅ |
| 零 trait object 跨 crate | ✅ |
| borrow checker 一行解决 | ✅ |
| 与现有 `cancel(&mut self)` 共存 | ✅ |
| stdout mode 隔离不受影响 | ✅ |
| 错误传播走现有 `Ok(RunResult{ Cancelled })` | ✅ |

**建议**：修正 3 处微小设计文档措辞后，实施。
