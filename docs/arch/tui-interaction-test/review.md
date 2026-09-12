# TUI 交互与显示可测试性 — 架构审查报告

> 审查对象：`docs/arch/tui-interaction-test/design.md` + ADR
> 审查方式：设计文档逐维度分析 + 代码假设验证

## 实施后回写（2026-09-12）

审查发现的 3 个 🔴 风险在实施中均已落地：
- 风险 1（CommandContext 19 构造点）→ 实际 17 处测试构造点统一加 `prompter: &StubPrompter`，未用 `for_test` helper（死代码已删）
- 风险 2（TuiSurface 注入矛盾）→ 因 T7 测试排除 scope，TuiSurface 未实现，矛盾自然消解
- 风险 3（PromptError 冗余）→ 合并为单一 `Cancelled`，已实现

## 总评

| 维度 | 判定 | 说明 |
|------|------|------|
| 可行性 | 🟢 绿 | 技术路径清晰，依赖成熟，无阻塞风险 |
| 可维护性 | 🟡 黄 | 3 个设计缺陷需修正，否则实施时会撞墙 |
| 可理解性 | 🟢 绿 | 文档结构好，两层拆分直观，ASCII 图清楚 |
| 性能与可靠性 | 🟢 绿 | 测试代码无性能关切；FakePrompter 无泄漏风险 |

## 风险排序

| # | 风险 | 影响 | 可能性 | 级别 |
|---|------|------|--------|------|
| 1 | CommandContext 加字段需改 19 个构造点（非设计声称的"所有构造点"未量化） | 高 | 确定 | 🔴 |
| 2 | TuiSurface trait 与"保持自由函数"矛盾——MockSurface 注入点不清晰 | 高 | 确定 | 🔴 |
| 3 | `PromptError::Interrupted` 与 `Canceled` 在生产中始终等价处理，多余变体 | 中 | 确定 | 🟡 |
| 4 | `make_test_view()` 函数不存在，实际是 `make_app()` | 低 | 确定 | 🟡 |
| 5 | `render_to_text` helper 已存在于 draw.rs，设计重复定义 | 低 | 确定 | 🟡 |
| 6 | 测试骨架断言 `agent.model_id()` 但实际应断言 `ctx.config.model` | 中 | 确定 | 🟡 |

## 逐维度分析

### 可行性 🟢

技术路径无问题：
- Prompter trait 对象安全（`&self` + 具体类型），`dyn Prompter` 可用
- inquire 4 个调用点已验证，替换为 `ctx.prompter.*` 一一对应
- TestBackend 渲染管线已验证，13 个现有测试证明可行
- FakePrompter 预设答案队列模式成熟，无技术障碍

**结论**：实施无阻塞，1 人天估算合理。

### 可维护性 🟡

#### 缺陷 1（🔴）：CommandContext 改造量被低估

设计声称"加字段 + 所有构造点传入 prompter"，但未量化。实际验证：

- **生产构造点**：1 处（`ui/mod.rs:245`）
- **测试构造点**：18 处（`builtin.rs` 17 处 + `mod.rs` 1 处）
- **总计 19 处**

所有测试构造点使用一致模式 `CommandContext { agent, config, state }`，但新增 `prompter` 字段后每处都需要改。

**建议**：引入测试 helper 函数减少改动：

```rust
// commands/mod.rs #[cfg(test)]
impl<'a> CommandContext<'a> {
    pub fn for_test(
        agent: &'a mut Agent,
        config: &'a mut Config,
        state: &'a mut StateStore,
        prompter: &'a dyn Prompter,
    ) -> Self {
        Self { agent, config, state, prompter }
    }
}
```

18 个测试构造点 → `CommandContext::for_test(a, c, s, &prompter)`，单行替换。

#### 缺陷 2（🔴）：TuiSurface 注入点矛盾

设计声称：
- "TuiSurface trait 主要服务于测试 MockSurface"
- "CrosstermSurface 不做独立结构体——dispatch_input 内部按泛型 B 直接调用现有自由函数"
- 但 `test_suspend_resume_paired` 测试需要在 dispatch_input 内部注入 MockSurface

**矛盾**：如果 dispatch_input 保持自由函数内联调用 suspend_terminal/resume_terminal，那 MockSurface 无处注入。要测 suspend/resume 成对性，必须在 dispatch_input 中有一个可替换的 TuiSurface 参数。

**建议**：两个选择：
- (a) dispatch_input 增加 `surface: &mut dyn TuiSurface` 参数（第 10 个参数，已很多）
- (b) 把 TuiSurface 测试降级为"观察 dispatch_input 的副作用"而非直接注入（如检查 raw mode 状态）

推荐 (a)——dispatch_input 已有 9 参数，加 1 个不影响（后续可能重构为参数 struct）。

#### 缺陷 3（🟡）：PromptError 变体冗余

设计定义 `PromptError::Canceled` 和 `PromptError::Interrupted`，但代码中 4 个 inquire 调用点**始终用 `|` 合并处理**：

```rust
Err(InquireError::OperationCanceled) | Err(InquireError::OperationInterrupted) => {
    println!("Login cancelled.");
    return Ok(CommandResult::Continue);
}
```

两个变体在生产中从未被区分处理。

**建议**：合并为 `PromptError::Cancelled`（一个变体），或保留两个但注明"当前等价，预留演进"。如果保留两个，FakePrompter 的测试骨架中应有断言证明当前确实等价处理。

#### 缺陷 4（🟡）：测试骨架断言不准确

设计 §4.7 中：

```rust
// /model 无参 → select model → model_id 切换
let result = ModelCommand.execute("", &mut ctx).await.unwrap();
assert_eq!(result, CommandResult::Continue);
```

断言太弱——只检查 Continue，不检查模型是否切换。实际应断言 `ctx.config.model == "gpt-4"`（现有测试已验证此路径）。

同样，`test_login_interactive` 断言 `config.registry.auth_for("openai").is_some()` 是正确的。

#### 缺陷 5（🟡）：render_to_text 和 make_test_view 不存在

- `make_test_view()` 不存在——实际测试用 `make_app()`（draw.rs:302），返回 `App` 而非 `AppView`
- `render_to_text()` 已存在于 draw.rs:334——设计中重复定义了一个相同函数

**建议**：第二层测试复用 draw.rs 现有的 `make_app()` + `render_to_text()`，不重复定义。

### 可理解性 🟢

文档质量高：
- 两层拆分（"脑子对不对" + "画面对不对"）用 ASCII 图直观表达
- 候选方案 A/B/C 对比清晰
- 改造前/改造后代码对比有效
- ADR 格式规范

**小问题**：§6 的"互补图"中 ModelCommand.select() 出现，但 `/model` 无参路径只走 Select，不走 Text/Password。图中可更精确。

### 性能与可靠性 🟢

- 测试代码无性能关切
- FakePrompter 使用 `Mutex<Vec>` 记录调用——单线程测试中 Mutex 无开销
- 无并发/死锁风险（测试串行执行）
- 资源管理：FakePrompter VecDeque 队列耗尽返回 Canceled，不 panic

## 改进建议汇总

### 易修复（实施时顺手改）

| # | 建议 | 涉及 |
|---|------|------|
| 1 | `make_test_view()` → 改为引用 draw.rs 的 `make_app()` | design.md §5.2 |
| 2 | `render_to_text` 复用 draw.rs 现有实现，不重复定义 | design.md §5.2 |
| 3 | 测试骨架 `test_model_interactive` 断言 `ctx.config.model` 而非只断言 Continue | design.md §4.7 |

### 需讨论（实施前确认）

| # | 建议 | 涉及 |
|---|------|------|
| 4 | PromptError 合并 Canceled + Interrupted 为一个变体，或保留但文档注明当前等价 | design.md §4.1 |
| 5 | 确认 TuiSurface 注入方案：dispatch_input 加 `&mut dyn TuiSurface` 参数 vs 降级为副作用观察 | design.md §4.3 |

### 架构级（文档更新）

| # | 建议 | 涉及 |
|---|------|------|
| 6 | 量化 CommandContext 改造量：19 个构造点 + 测试 helper 函数设计 | design.md §7 |
| 7 | 明确 TuiSurface trait 的注入点——当前设计中 MockSurface 无处注入 | design.md §4.3 |

## 结论

设计整体可行，两层拆分思路正确。3 个缺陷（#1 改造量低估、#2 TuiSurface 注入矛盾、#3 变体冗余）需在实施前修正，否则实施时会撞墙。建议更新 design.md 后再进入编码。
