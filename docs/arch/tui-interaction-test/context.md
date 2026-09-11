# TUI 交互行为可测试性 — 架构上下文

## 概述

回答一个问题：slash 命令的**交互行为**（`inquire` 输入 + TUI suspend/resume 让位）能否放进 coding-agent（TUI 宿主）自己的包内自动化测试，替代"真终端人工验"。

结论（预览）：**能，但不是直接测真实 inquire / 真实终端**——真实交互在进程内不可伪造；可行路径是**抽象注入 + 单测**：`Prompter`（命令的输入面）与 `TuiSurface`（suspend/resume 控制面）两个小 trait，生产实现薄壳包 `inquire`/`crossterm`，测试实现记录调用/预设答案，把交互行为的**控制逻辑**稳定测进 `cargo test`；字节级真 tty 行为留少数 `#[ignore]` 或人工。

## 现有架构

### 模块边界

- TUI **不是独立 crate**：`ui/` 是 `apps/coding-agent`（bin crate，package `yushan-coding-agent`）内的模块，feature `tui-ratatui`。
- 命令在 `apps/coding-agent/src/commands/`（builtin.rs），经 `CommandContext { agent, config, state }`（commands/mod.rs:41-45）执行，`execute(&self, input, &mut ctx) -> Result<CommandResult, CommandError>`。
- `dispatch_input` slash 分支（ui/mod.rs:237-258）：`suspend_terminal → commands.execute → resume_terminal`（提交 ec5e57c）。

### 核心抽象（与可测试性直接相关）

| 抽象 | 形态 | 测试障碍 |
|------|------|----------|
| `suspend_terminal` / `resume_terminal` | `fn<B: Backend + Write>(&mut Terminal<B>)`，`execute!` 写 ANSI + raw mode 开/关 | `TestBackend` 只 `impl Backend`，**不 `impl Write`** → 泛型 `+ Write` 约束挡死 TestBackend；`Terminal<CrosstermBackend<Vec<u8>>>::new` 会调真实 `size()`，假 writer 炸 |
| 命令交互 | `inquire::Select`/`Password` **硬编码**在 builtin.rs（/login:170·219·242，/model:415） | inquire 同步阻塞、自带终端接管；进程内无 fake；无 pty/expect 类 crate |
| 测试基建 | draw.rs/events.rs 用 `TestBackend` 单测；`tests/integration.rs` 用 MockModel；`e2e_tools` `#[ignore]` | dev-dependencies 为空；无 `rexpect`/`portable-pty`/`expectrl` |

### 关键数据流

```
TUI 提交 /login → dispatch_input
     → suspend_terminal（EnableLineWrap + raw off + Show + ?1049l 离 alt）
     → commands.execute → inquire 在真实终端交互（自接管 stdin/stdout）
     → resume_terminal（raw on + ?1049h 重进 + DisableLineWrap + terminal.clear()）
     → 返回 chat 帧
```

### 外部依赖

| 依赖 | 版本 | 用途 | 备注 |
|------|------|------|------|
| inquire | 0.8 | /login /model 的选择/密码 | 直接操作终端，进程内不可伪造 |
| ratatui | 0.29 | 渲染 + TestBackend | TestBackend 非 `Write`；`Terminal::new` 需真实尺寸 |
| crossterm | 0.28 | 事件/raw/alt | `EventStream` 后台线程仅轮询时读 stdin（ec5e57c 核过：命令期间不抢 stdin） |
| pty/expect | — | 无 | 需真实 tty 测试才引入 |

## 约束

- **技术**：无 pty dev-dep；TestBackend 不满足 `+ Write`；`inquire` 同步阻塞不可注入；命令把 inquire 硬编码。
- **性能/稳定**：包内单测须快且并行安全；pty 测试并行易抖，只能 `#[ignore]`。
- **演进**：CLAUDE.md「数据与显示分离」——命令本应 **display-agnostic**（不硬绑某个交互实现），Prompter 抽象正好落回这个边界；不越界到动态插件。
- **组织**：项目处「最小闭环 → 可用适配器」阶段；既有测试范式 = TestBackend 单测 + integration + `#[ignore]` e2e，新抽象应延续。

## 需求范围

### 范围内（可放进 coding-agent 包内单测）

- **`Prompter` trait**：把 `/login` `/model` 的 `inquire::Select/Password` 抽成 `trait Prompter { select / password … }`。生产 impl 包 `inquire`（仍在 suspend 后的真实终端跑）；测试 impl 返回预设答案 → 命令逻辑（选 provider、存 auth、切模型）可单测。
- **`TuiSurface` trait**：把 `suspend_terminal`/`resume_terminal` 抽成 `trait TuiSurface { suspend(&mut self); resume(&mut self); }`。真实实现作用于 `Terminal<CrosstermBackend<Stdout>>`；测试实现记录调用序列 → 断言「slash 分支恒 suspend→resume 成对」「报错路径也 resume」「resume 后清屏」。
- **`dispatch_input` 行为单测**：fake Prompter + fake TuiSurface 注入，覆盖 /login、/model、未知命令、取消、/quit。

### 范围外（不放进包内单测）

- **真实 inquire 在真 tty 的交互**（方向键选择、密码不回显）——只能 pty（新增 dev-dep `portable-pty`/`rexpect` + `#[ignore]`）或人工。
- **suspend 期间字节级证据**（crossterm 线程确实不抢 inquire 的 stdin）——et io 集成/人工，ec5e57c 已源码级核过，不必机器复验。

### 关键场景

- 场景 1：`/login`（无参）+ fake prompter 依次返回 provider / api key → 断言 auth 已持久化、`Config` 更新。
- 场景 2：`/model`（无参）+ fake prompter 返回某个 model → 断言 `agent.model_id()` 切换。
- 场景 3：任意 slash 命令 → 断言 fake surface 被调用 `suspend → resume`（含命令执行报错时也 resume）。
- 场景 4：prompter 返回 `Canceled` → 断言 `TranscriptLine::Error/System`（"Login cancelled."）进入 transcript，TUI 正常回 chat。

### 边界场景

- 未知命令 `Err` → resume 不丢失、错误进 transcript。
- `/quit` → suspend→resume→should_quit 置位（最终由外层 break + restore）。
- `suspend` 失败 → 传播错误（当前 `?` 语义），不进入执行。

## 未澄清问题

- [ ] `Prompter`/`TuiSurface` 放哪里：`commands/` 内 trait vs `ui/` 模块内 trait？`CommandContext` 增 `&dyn Prompter` 字段 vs 独立参数（后者改动最小，但让 `execute` 签名加参）？
- [ ] 值得为真实 tty 交互引 `portable-pty`/`rexpect` dev-dep 吗（收益 vs 依赖体积/脆弱性）；还是 Prompter 假实现 + 少量人工验足够？
- [ ] 先抽「纯」逻辑（如 `/model` 无参 → 决策需要 select）把 inquire 依赖压到最小注入面，还是全量抽 trait？
- [ ] 命令逻辑单测放模块内（builtin.rs `#[cfg(test)]`）还是 `tests/` 集成？前者贴近现有 draw.rs 风格。

## 后续建议

- 用 `arch-design` 出 `Prompter`/`TuiSurface` 两个抽象的具体设计（trait 形状、注入点、fake 测试形态），再行实施。
- 用 `tdd`：先写 fake 驱动的命令单测（红）→ 再引入 trait（绿）→ 收敛生产 impl。
- 交互「控制逻辑」进包内单测后，真 tty 行为只需 `#[ignore]` 或人工轻量兜底即可。