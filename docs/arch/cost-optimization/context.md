# cost-optimization — 架构上下文

> 日期：2026-10-27
> 状态：**需求已澄清，待方案设计**（下一步 `arch-design`）。
> 缘起：一次 9 turn / 159 次模型往返的会话把 MiMo 余额耗尽
> （`1793068612_812222000`，复盘数据见 `docs/arch/explore-speed/codewhale-cost-research.md` §4）。
> 竞品对照见同目录外的 `docs/arch/explore-speed/codewhale-cost-research.md`。

## 概述

让「一次会话花掉多少钱」在**发生时就可见**，并给随轮数线性放大的成本项加一个与窗口大小
无关的上界；同时用提示词降低「模型自己直连 API 烧钱」与「凭据落进命令」两个概率。

## 现有架构

### 模块边界

三条优化点各自落在不同层，互不共用代码：

| 关注点 | 层 | 关键文件 |
|---|---|---|
| usage 解析 | adapter | `adapters/model-openai-compatible/src/{compat,stream,response}.rs` |
| usage 累计 | app | `apps/coding-agent/src/status.rs`（`TurnStats`） |
| usage 展示 | TUI | `crates/ys-tui-coding/src/{view,draw}.rs` |
| 压缩阈值 | 组件（纯函数） | `crates/ys-component/src/compact.rs` |
| 压缩触发 | loop | `crates/ys-loop/src/basic.rs` |
| 提示词 | app | `apps/coding-agent/src/prompt.rs` |

### 核心抽象

**`Usage`**（`crates/ys-core/src/usage.rs:3`）—— 逐轮与累计两用：

```rust
pub input_tokens: u32,
pub output_tokens: u32,
#[serde(default)] pub prompt_cache_hit_tokens: u32,
#[serde(default)] pub prompt_cache_miss_tokens: u32,
#[serde(default, skip_serializing_if = "Option::is_none")] pub reasoning_tokens: Option<u32>,
```

注意 `reasoning_tokens` 已经是 `Option`，且注释（`:18`）明确「`None` = 未上报；
`Some(0)` = 上报了真 0。两者语义不同，不能合并」—— **两个 cache 字段没有遵循同一条
已在本仓确立的规则**，这是第 3 项要修的语义不一致。

**`CompactPolicy`**（`crates/ys-component/src/compact.rs:18`）—— `#[non_exhaustive]`
纯数据 + 纯函数：

```rust
pub fn trigger_at(&self, window: usize) -> usize   // :45
pub fn keep_budget(&self, window: usize) -> usize  // :57
```

现有不变量（`compact.rs:182-211`，`P1`）：
**`trigger_at(w) > keep_budget(w)` 对任意 `w ≥ 1` 必须成立**。
违反它 `keep_from` 恒为 0，压缩每轮空转。这是任何新阈值必须保住的性质。

**`ApprovalHandler`**（`crates/ys-tool/src/approval.rs:14`）—— 扩展点**已存在**：

```rust
fn needs_approval(&self, tool_name: &str, input: &Value) -> bool;
async fn request_approval(&self, tool_name: &str, input: &Value) -> ApprovalDecision;
```

loop 侧接线**已存在**（`crates/ys-loop/src/basic.rs:965-967`），但两个装配点都装的是
`AutoApprove`（`apps/coding-agent/src/toolset.rs:258`、`apps/coding-agent/src/main.rs:1024`，
恒放行）。本需求**不使用**它，记录在此是因为讨论中曾考虑过「bash 层拦截」，见「范围外」。

### 关键数据流

**usage 从 provider 到屏幕（第 1 项要打通的路）**

```text
SSE 末包 usage  ──► ChatUsage::cache_tokens() 归一化
                 └─► ys-loop 逐轮累加 → RoundUsage 事件（带完整 Usage，已可用）
                     └─► TurnStats::record()       apps/coding-agent/src/status.rs:28
                         └─► 已累计 total_cache_hit_tokens / total_cache_miss_tokens（:11-13）
                             └─► ✗ 断裂：CodingView 无对应字段
                                 apps/coding-agent/src/view.rs:114 附近只推 ↑↓ 总量
                                 └─► ✗ 断裂：draw.rs:768 总量为 0 时整段不渲染
```

**结论：数据已在 app 侧，缺的只是「进 View」与「恒渲染」两处**，不需要新事件通道。

**压缩触发（第 2 项要改的路）**

```text
RunLimits.compaction (CompactPolicy)
  └─► basic.rs:515  let trigger = policy.trigger_at(window);
      └─► 与 estimate_session_tokens + fixed_prompt_tokens 比较后决定是否压缩
```

`trigger_at` 是**纯函数，只看 `window`，看不到实际已用了多少** —— 这是第 2 项的核心困难。

### 外部依赖

| 依赖 | 用途 | 备注 |
|---|---|---|
| `serde` | `Usage` / 事件 JSON | 字段加 `Option` 属向后兼容的加法式改动 |
| provider 的 `stream_options.include_usage` | 是否上报 usage | minimax 当前**关闭**，见下 |
| （外部项目）seabox | 出站网络策略 | 本仓不实现，见「范围外」 |

## 约束

- **技术**
  - `ys-core` / `ys-event` / `ys-protocol` **不含 tokio**（`AGENTS.md` 硬性约束）⇒
    `Usage` 与 `CompactPolicy` 的改动必须是纯数据 / 纯函数。
  - **UI 线程必须无 runtime 上下文**：TUI 用 `blocking_send`，在 async 上下文会 panic
    （`AGENTS.md`）。任何「app 向 UI 请求交互」的设计都受此约束。
  - `CompactPolicy` 是 `#[non_exhaustive]` ⇒ 加字段是加法式改动，不破坏既有构造。
  - `apps/coding-agent/src/config.rs:643` 已注明「`CompactPolicy::trigger_at` 自己会用
    `output_reserve` 钳住」⇒ 新阈值必须沿用同一钳制，不能绕开它。
- **语义**
  - `Usage` 的字段一旦改 `Option`，**累加语义必须定义**：照 `reasoning_tokens`
    的既有规则（`usage.rs:35-39`）—— 仅两侧都上报才相加，否则继承已上报的一侧。
  - `#[serde(default)]` 的旧 JSON 必须仍能反序列化（老会话文件、老日志）。
- **行为兼容**
  - 压缩阈值改动会改变**既有会话的压缩时机** ⇒ 需要 `config.json` 可覆盖，
    且缺省行为要能说清是「变了」还是「没变」。
  - 提示词改动会改变**前缀字节** ⇒ 一次性破所有用户的缓存。此项须与
    「前缀指纹锚」测试（`apps/coding-agent/src/toolset.rs:572`）一起评估。
- **组织**
  - 网络出口拦截归 seabox，本仓**不实现、不定义抽象**（`docs/design.md:52`）。

## 需求范围

### 范围内

**1 · 成本可见性（最高优先）**

- `compat.rs:63` 的 `minimax()` 打开 `supports_stream_usage`。
  **已知风险**：该 provider 对未知顶层字段是否放行「未经验证」（注释原文）。
  **已确认的安全网**：适配器在识别到端点拒绝 `stream_options` 时**剥掉该字段重发一次**
  （`adapters/model-openai-compatible/src/lib.rs:603-639`），故打开开关不需要新机制；
  失败路径要落进 `--debug`（`meta.fallback_retry` 已存在）。
- `Usage` 的 `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` 改
  `Option<u32>`，与 `reasoning_tokens` 同规。
- `CodingView` 增加缓存命中字段（数据源 `TurnStats`，已累计）；
  状态栏显示命中率，且**总量为 0 时不再整段隐藏**（`draw.rs:768`）——
  要把「没有读数」与「读数是 0」显示成两种不同的东西。

**2 · 压缩触发加绝对上限**

- 给 `CompactPolicy` 加一个与 `window` 无关的绝对上限字段。
- 取值规则：**`min(trigger_at(window), cap)`，并保证结果 ≥ `keep_budget(window) + 1`**
  （即保住 P1 不变量；小窗口下 `cap` 实际不生效，行为不变）。
- `cap` 要可经 `config.json` 覆盖。

**3 · 提示词两条**

- **费用事实陈述**：直连模型 API 端点会产生真实费用，且**不进入本会话的 usage 统计**；
  要探测 provider 能力应改代码跑测试，不要在 bash 里 curl 它。
- **凭据禁令**：不要把 API key / token 写进 bash 命令或临时文件（会留在会话记录与日志里）。
- 两条都按既有形态写：**陈述事实 + 说明代价 + 给替代路径**，
  先例是 `prompt.rs:279` 的 webfetch 条目（末尾刻意写明「多一次模型调用、更慢」）。

### 范围外（明确不做的）

- **bash 层的命令字面识别与拦截 —— 已否决**。否决理由见下节「被否决的方案」。
- **出站网络拦截**：归 seabox。本仓不定义沙箱/网络策略抽象
  （`docs/design.md:52`、`AGENTS.md:110`）。
- **按金额/按 token 的硬性中止**：本次不做。理由：coding agent 中途停下的代价
  常高于多花钱；且要先有第 1 项才有读数判断阈值。
- **PTC / 代码模式**：`docs/TODO.md` 已定「本轮明确不做」，需先有执行边界。
- **调小 `context_window` 作为降本手段**：单独做**有害**，见「关键场景 · 异常边界」。

### 被否决的方案（留档，防后人重提）

**bash 命令字面识别 + 运行时拦截 + 用户确认**

曾提议在 bash 层识别「是否在向模型 API 端点发请求」并拦截。**否决**，实测依据：

用 8 种形态测试字面匹配（`xiaomimimo` / `chat/completions`），只能抓到 2 种：

| 形态 | 能否识别 |
|---|---|
| `B=https://api.xiaomimimo.com; curl $B/v1/...` | ✅ |
| 配置文件里的 URL（`probe.py c.json`） | 部分（字面在 json 内） |
| `echo aHR0cHM6... \| base64 -d \| xargs curl` | ❌ |
| `python3 -c "...'api.'+'xiaomimimo'+'.com'..."` | 部分 |
| `pip install some-sdk && some-sdk --prompt ...` | ❌ |
| `curl http://127.0.0.1:8080/v1/chat/completions`（本地代理） | ❌ |
| `curl "$OPENAI_BASE_URL/chat/completions"` | ❌ |
| `curl https://api.openai.com/...`（换个端点，同样烧钱） | ❌ |

**更关键的是：这次事故的会话里模型读过 `~/.yushan/auth.json`**
（turn 4 round 1 有 `cat ~/.yushan/auth.json`）。故它下次完全可能写
`curl $(jq -r .minimax.api_base ~/.yushan/auth.json)/v1/chat/completions` ——
**不是恶意规避，是更顺手**。字面匹配对此必然漏。

误报与漏报的代价不对称：误报只是多按一次放行（可接受），
漏报等于事故重演（不可接受）。任何基于命令字面的规则都做不到「不漏」⇒ 否决。

**「运行时识别」这个前提不成立，也就没有可依赖的识别层**。
基于网络出口的判定不属于本仓（见范围外）。

## 关键场景

- **场景 1（成本可见）**：用户跑一个长任务，状态栏实时显示累计输入/输出与**缓存命中率**。
  provider 上报了 0 命中 ⇒ 显示 `0%`；provider **未上报** ⇒ 显示「无读数」而非 `0%`。
- **场景 2（压缩上限生效）**：1M 窗口、`cap = 60k` 时，第 5 轮上下文到 60k 即压缩，
  而不是等到 850k（现状 `trigger_at(1_000_000) = 850_000`）。
- **场景 3（小窗口不受影响）**：`window = 32_768` 时 `trigger_at = 27_648`
  （`keep_budget = 3_276`），`cap` 高于它 ⇒ 取 `trigger_at`，**行为与改前逐字节相同**。
- **场景 4（提示词生效）**：模型为探测 provider 能力而想 curl 时，
  看到「会产生未纳入统计的费用 + 去哪查」后改用代码路径。

### 异常 / 边界

- **`cap` 低于 `fixed_prompt_tokens`**：会在第一轮就触发压缩，而压缩无从下手
  （消息已到底）。`apps/coding-agent/src/main.rs:1091` 的 `fixed_cost_note` 只**诊断**
  不拦截 ⇒ 需要新增一条启动期提示，或让 `cap` 自动抬到 `fixed_prompt_tokens` 之上。
- **`window` 配小后的钳制区**：`trigger_at` 是 `min(window × 0.85, window − 5120)`
  （`output_reserve 4096` + `CONTEXT_HEADROOM 1024`，`compact.rs:11,47`）。
  实测本仓的拐点在 `window ≈ 20_000` 以下：

  | window | trigger_at | 占窗口 |
  |---|---|---|
  | 65_536 | 55_705 | 85.0%（未受钳制）|
  | 32_768 | 27_648 | 84.4%（未受钳制）|
  | 20_000 | 14_880 | 74.4% |
  | 8_192 | 3_072 | 37.5% |
  | 4_096 | 410 | **10.0%** |

  ⇒ **「调小 `context_window`」不能当作降本手段**：窗口降到 ~20 K 以下时，
  触发点会被预留量钳死，压缩开始频繁空转。

  > ⚠️ 与 Codewhale **形态相同但严重程度不同**：它用 64 K 输出预留，
  > 拐点在 ~328 K 以上就已生效、小窗口被钳到 1,024 token（#6540）。
  > 本仓预留只有 5 K，拐点在 ~20 K —— 不要把它的数字直接套到本仓。**minimax 打开 usage 后仍返回 400**：走既有回退（`lib.rs:603`），
  该次运行 usage 记为「无读数」而非 0。
- **旧会话 JSON 无缓存字段**：`#[serde(default)]` ⇒ 反序列化为 `None`（未上报），
  而不是 `Some(0)`。

## 未澄清问题

- [ ] **`cap` 的缺省值取多少？** 需要真实读数支撑 —— 建议先落第 1 项、跑若干真实会话，
      用 `cache_miss` 的分布来定，而不是拍一个数。**待用户确认**。
- [ ] **`cap` 对「所有模型」还是「窗口 ≥ 阈值才启用」？**
      已定形态是 `min(trigger_at, cap)` + 保 P1 下界（方案 B），这在语义上等价于
      「所有模型都配，但小窗口自动不生效」，无需再选。**待用户确认这个理解是否正确**。
- [ ] **提示词改动要不要与其它前缀改动合并发一次？**
      两者都会破一次缓存前缀；分开做等于破两次。**待用户确认**。
- [ ] **`cap` 是否应该同时钳住 `max_turn_tool_result_tokens`？**
      后者当前是 `window/4`（`apps/coding-agent/src/main.rs:1053`），
      大窗口下同样线性放大。**待用户确认是否纳入本次范围**。
- [ ] **seabox 侧的交接内容**：需向对方点明两点 ——
      ① 受控端点清单建议来自 `auth.json` 的 `api_base`（不硬编码厂商列表）；
      ② **本地回环是残留风险**（`127.0.0.1:8080` 转发到远端，网络层只看到本地连接），
      需在那边定「回环是否放行」策略。**待用户确认由谁去交接**。

## 后续建议

- 用 `arch-design` 做方案设计。三件事的耦合度低，建议**拆成三个独立设计单元**，
  但第 1 项必须先落地（它是另两项的验收前提）。
- 第 2 项**必须做变异测试**：把 `trigger_at` 的 `min` 去掉、或去掉保 P1 下界，
  都应让既有 `test_p1_*` 用例变红。这是本仓对关键逻辑的既定要求
  （`AGENTS.md`「关键逻辑要做变异测试」）。
- 第 3 项（提示词）建议先单独提交，因为它会破一次前缀缓存；
  与第 1、2 项混在一起会让「命中率变化」与「压缩时机变化」互相污染读数。
- 用 `prototype` 验证一个假设：`UseChurn` —— 把 `Usage` 改 `Option` 后，
  真实会话里 **未上报的比例有多高**。这直接决定成本数字可信到什么程度，
  目前完全没有数据。
