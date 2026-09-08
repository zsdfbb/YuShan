# TUI 改进总览 — 架构上下文

## 概述

本目录覆盖三波 TUI 改进需求，由用户多次提出合并而成：

1. **常驻状态栏** — 每次 prompt 前打印一行 footer，显示当前 provider · model · cwd · 累计 tokens
2. **持久化「上次活跃 provider + model」** — 记到 `~/.yushan/state.json`，启动时优先恢复
3. **P1/P2/P3 改进清单** — 11 项低成本高 ROI 的 TUI 显示改进（banner 加字段、命令输出精简、Ctrl-C、turn 耗时、context %、输入补全等）

第 3 项的完整列表见 [`improvements-p1-p2-p3.md`](./improvements-p1-p2-p3.md)；本文档聚焦**第 1 + 2 项的现状与设计**。

---

## 现有架构

### TUI 现状（d73054f 后）

`apps/coding-agent/src/tui.rs:10-82`：

```
启动 banner（4 行）
  ↓
loop {
  print!("> ");            ←  提示符，无状态
  read_line();
  分发命令 / agent turn
}
```

中间没有任何状态输出。已实现：
- ✅ 启动 banner（format::print_banner）
- ✅ turn summary（format::print_turn_summary）
- ❌ 常驻 footer（每次 prompt 前看不到 model）
- ❌ Tool 调用过程（agent 调 bash 时用户看不到）
- ❌ Ctrl-C 优雅中断

### 持久化现状

| 文件 | 内容 | 写入路径 |
|------|------|---------|
| `~/.yushan/auth.json` | `HashMap<provider_name, AuthEntry{api_base, api_key, model}>` | `/login` + `/logout` |

实际结构：

```json
{
  "deepseek": {
    "api_base": "https://api.deepseek.com",
    "api_key": "sk-...",
    "model": "deepseek-chat"
  }
}
```

`AuthEntry.model` 是 `/login` 时写入的 provider 默认 model；`/model` 切换时**不更新**它（builtin.rs:380-413 只改 `config.model`）。

### 启动恢复现状（main.rs:24-38）

```rust
config.registry.load_auth();
if !config.is_configured() {
    for provider in config.registry.providers() {
        if let Some(entry) = config.registry.auth_for(&provider.name) {
            config.api_base = Some(entry.api_base.clone());
            config.api_key = Some(entry.api_key.clone());
            config.model = entry.model.clone();           // ← 用 provider 内 model
            config.provider = Some(provider.name.clone());
            break;                                          // ← 选第一个有 auth 的
        }
    }
}
```

**两个真实缺口**：
- 总是选**第一个有 auth 的 provider**，不区分「上次活跃」
- 恢复的是 provider 内默认 model，不记住用户 `/model` 切换过的

### 可用数据源

| 数据 | 来源 | 获取方式 |
|------|------|---------|
| 当前 model | `Agent::model_id()` | `agent.model_id()` |
| 当前 provider | `Config::provider` | `config.provider.as_deref()` |
| cwd | `Config::cwd` | `prompt::format_cwd_tilde(&config.cwd)` |
| 累计 tokens | `TurnStats` | `stats.total_input_tokens` 等 |
| 已登录 provider 列表 | `ProviderRegistry` | 需 pub `logged_in_count()` |
| 已注册命令列表 | `CommandRegistry` | `commands.all()` 已存在，含 name/desc/arg_hint |

---

## 用户决策（已收集）

| 决策点 | 选择 |
|--------|------|
| 常驻栏位置 | 顶部或底部常驻（每次 prompt 前 print 一行） |
| 持久化字段 | provider + model 都记 |
| 输入历史（↑↓） | 引入 rustyline = "14" |
| session 起点 | 本次会话时间（main.rs 启动时记 Instant::now()） |
| 常驻栏 ANSI 颜色 | 暂不引入依赖，必要时 inline ANSI |

---

## 设计方案

### 1. 常驻状态栏

**形态**：每次 prompt 前打印一行，类似 Pi 简化版 footer：

```
┌─ ~/Develop/YuShan · deepseek · deepseek-chat · ↑320 ↓1.2k ─┐
> _
```

**技术选型 — 方案 A**（最简，无 ANSI）：
- 每次循环开头 `print!("\n{footer}\n> ")`，不覆盖、不上移
- 优点：纯 print，与现有 d73054f banner/summary 风格一致
- 缺点：终端滚动时 footer 会被卷上去（但 readline 的 history 体验能掩盖这点）

**位置选择**：在 prompt 之前独立一行，与 banner/turn summary 风格统一。

**字段选择**（user 已决策常驻）：
- cwd（`format_cwd_tilde`）—— banner 已有但 footer 再提强化
- provider · model
- 累计 ↑↓tokens

**截断处理**：终端宽度不足时按 ANSI 感知的可见宽度截断（不引入 `unicode-width` crate，手工 byte index 即可）。

### 2. 持久化 last_active

**新增文件**：`~/.yushan/state.json`（与 auth.json 同目录，0o600 权限）

**Schema**：

```rust
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct AppState {
    pub last_active_provider: Option<String>,
    pub last_active_model: Option<String>,
}
```

**新模块**：`apps/coding-agent/src/state.rs`

```rust
pub struct StateStore { path_override: Option<PathBuf> }
impl StateStore {
    pub fn new() -> Self;
    pub fn load(&self) -> AppState;        // 文件不存在 → Default::default()
    pub fn save(&self, state: &AppState) -> Result<(), String>;
}
```

**写时机**：

| 触发 | 写什么 |
|------|--------|
| `/login` 成功 | `last_active_provider = provider.name`, `last_active_model = provider.default_model` |
| `/model XXX` 切换 | `last_active_model = "XXX"`, `last_active_provider` 不变 |
| `/logout` | 两个字段都置 `None` |

**启动恢复升级**（main.rs）：

```rust
config.registry.load_auth();
let state = StateStore::load();

if !config.is_configured() {
    // 优先用 state.json 恢复（带 fallback）
    let last = state.last_active_provider.as_deref()
        .and_then(|n| config.registry.find_provider(n))
        .zip(state.last_active_provider.as_deref()
            .and_then(|n| config.registry.auth_for(n)));

    if let Some((provider, entry)) = last {
        config.api_base = Some(entry.api_base.clone());
        config.api_key = Some(entry.api_key.clone());
        // 优先用 state.json 的 model（用户切换过），fallback 到 entry.model
        config.model = state.last_active_model.clone()
            .unwrap_or_else(|| entry.model.clone());
        config.provider = Some(provider.name.clone());
    } else {
        // 现有逻辑：第一个有 auth 的 provider
        for provider in config.registry.providers() {
            if let Some(entry) = config.registry.auth_for(&provider.name) {
                config.api_base = Some(entry.api_base.clone());
                config.api_key = Some(entry.api_key.clone());
                config.model = entry.model.clone();
                config.provider = Some(provider.name.clone());
                break;
            }
        }
    }
}
```

### 3. P1/P2/P3 改进清单

完整 11 项清单见 [`improvements-p1-p2-p3.md`](./improvements-p1-p2-p3.md)。摘要：

**P1（高 ROI，5 项）**：banner 加工具列表 / banner 加 config 路径+版本 / `/login` 输出精简 / `/model` 输出精简 / Ctrl-C 中断反馈

**P2（中 ROI，4 项）**：turn 耗时 / context 占比 / 错误时显示来源 / session 起始时间

**P3（体验优化，6 项）**：`/help` 增强 / 输入历史（rustyline）/ 多行输入 / ANSI 颜色 / first-run 欢迎语 / session 计时在 footer

**输入补全**（user 本轮新增）：rustyline 自定义 Completer 读 `Command::name/description/arg_hint`，按 Tab 补全 + 行内 ghost text 描述。**与 P3-13（输入历史）共用 rustyline**——一举多得。

---

## 关键文件改动（汇总）

| 文件 | 改动 | 来源 |
|------|------|------|
| `apps/coding-agent/src/state.rs` | **NEW** — AppState + StateStore | 设计 2 |
| `apps/coding-agent/src/tui/completer.rs` | **NEW** — CmdCompleter (rustyline Completer) | 设计 3-P3-13 |
| `apps/coding-agent/src/format.rs` | MODIFY — 新增 `print_footer` 函数 | 设计 1 |
| `apps/coding-agent/src/tui.rs` | MODIFY — footer 调用 + rustyline 替换 read_line + Ctrl-C 处理 | 设计 1 + 3 |
| `apps/coding-agent/src/main.rs` | MODIFY — 加载 StateStore；恢复流程升级；构造 completer entries | 设计 2 |
| `apps/coding-agent/src/commands/builtin.rs` | MODIFY — `/login` `/model` `/logout` 写 state.json；输出精简 | 设计 2 + 3-P1 |
| `apps/coding-agent/src/format.rs` | MODIFY — banner 加 Tools 行 / Config 行 / 版本号 | 设计 3-P1 |
| `apps/coding-agent/src/provider.rs` | MODIFY — pub `auth_path()`、`logged_in_count()` getter | 设计 3-P1 |
| `agent-runtime/src/agent.rs` | MODIFY — pub `tool_names()`、`context_window()`、`cancel()` | 设计 3-P1/P2 |
| `apps/coding-agent/Cargo.toml` | MODIFY — 加 `rustyline = "14"` | 设计 3-P3 |

---

## 约束

- **技术**：纯 stdin/stdout + rustyline 行编辑，不引入 ratatui/crossterm
- **兼容**：`auth.json` 现有结构不动；state.json 缺失时降级到「第一个有 auth 的 provider」
- **最小化**：v0 常驻栏只展示 cwd · provider · model · 累计 tokens
- **API 扩展**：见 `improvements-p1-p2-p3.md` — `Agent` 公开 API 加 3 个 getter + 1 个 setter（cancel），需要 ADR 标注

---

## 关键场景

### 场景 1 — 启动常驻状态栏

```
┌─ ~/Develop/YuShan · deepseek · deepseek-chat · ↑0 ↓0 ─┐
> _
```

### 场景 2 — Turn 完成后

```
[AI 回复文本...]

✓ 1 round · ↑320 ↓1.2k tokens
┌─ ~/Develop/YuShan · deepseek · deepseek-chat · ↑320 ↓1.2k ─┐
> _
```

### 场景 3 — 命令补全

```
> /mo█
     ↓ (Tab)
> /model █
     ↓ (描述 ghost text: "Show or switch the current model")
```

### 场景 4 — `/login minimax` 切换 provider

```
> /login minimax
[交互... API key 输入]
✓ Logged in to minimax (MiniMax-Text-01).
┌─ ~/Develop/YuShan · minimax · MiniMax-Text-01 · ↑0 ↓0 ─┐
> _
```

（state.json 已写入 `last_active_provider=minimax`）

### 场景 5 — 重启恢复

| 上次状态 | 重启后 |
|---------|--------|
| `/login minimax` + `/model abab-7` | 自动登录 minimax + abab-7，footer 正确 |
| `/logout` | `(not configured)`，无 provider 恢复 |
| state.json 缺失但 auth.json 有 | 走 fallback：第一个有 auth 的 provider |

---

## 未澄清问题

### 设计 1（常驻 footer）

- [ ] footer 中是否包含 cwd（banner 已有）？倾向**包含**——用户希望一眼看到 cwd
- [ ] footer 风格用 `┌─ ... ─┐` box-drawing 还是简洁分隔符 `·`？倾向**两者结合**——box-drawing + 内部 `·` 分隔

### 设计 2（state.json）

- [ ] `/model XXX` 切换到不存在的 model 时，state.json 是否记录？倾向**记录**——让下次启动尝试恢复
- [ ] state.json 缺失时 fallback 是「第一个有 auth」还是「未配置」？倾向**第一个有 auth**（兼容现有行为）

### 设计 3（P1/P2/P3）

- [ ] rustyline 引入是否独立 PR？还是与 P1/P2/P3 一并？
- [ ] ANSI 颜色（设计 17）是 P3 是否同步做？

---

## 后续建议

1. **本轮**：`/arch-design` 设计完整实施计划，整合 3 个设计为一个 design.md
2. **落地分组**（与 improvements-p1-p2-p3.md 一致）：
   - 分组 A（低成本，零 API 扩展）：turn 耗时 / 错误来源 / session 时间 / 输出精简
   - 分组 B（小幅 API 扩展）：banner 加工具列表 + config 路径
   - 分组 C（state.json + 常驻 footer）：本目录主线
   - 分组 D（rustyline）：含命令补全 + 历史 + 多行 + 简化 Ctrl-C
   - 分组 E（context 占比 + Ctrl-C）：需 `Agent::context_window/cancel()`
3. **验证**：每个分组跑 `cargo test` + 手动验证

---

## 关联文档

- [`improvements-p1-p2-p3.md`](./improvements-p1-p2-p3.md) — 11 项 P1/P2/P3 详细清单
- `docs/arch/tui-status-display/` — d73054f 已实现的 banner + turn summary（前一阶段）
- `tmp/pi/packages/coding-agent/src/modes/interactive/components/footer.ts` — Pi footer 参照
