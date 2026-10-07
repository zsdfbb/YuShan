# session-default-new — 设计方案

> 需求与约束见 [`context.md`](./context.md)（决策 D1–D13）；业界对照见 [`research.md`](./research.md)；
> 决策记录见 [`docs/adr/0026-session-default-new-and-resume.md`](../../adr/0026-session-default-new-and-resume.md)。

## 1. 问题一句话

交互式 TUI 启动时**无条件恢复最近会话**（`Wiring::persistent` → `latest_session_path`），
用户无法「开一个新的」。本设计把默认行为改为**新会话**，并用**显式 `--resume`** 提供恢复能力。

## 2. 业界调研（摘要，详见 `research.md`）

| 参考 | 对本设计的直接输入 |
|---|---|
| **Pi** | 默认新会话；`--resume/-r`（picker）与 `--session <id\|path>`（指定）二分 → 本设计的 `Resume::Pick / Id` |
| **CodeWhale** | `/resume [id\|path]` 无参 → picker；运行中禁止 resume → 支撑 D11「启动期一次性」 |
| **Codex** | 列表按 `updated_at DESC`；`title == first_user_message`（存原文、显示时截断）→ 本设计用 **mtime DESC** + meta 存**有界片段**（见 §5.2 修订） |

三家标题**都是机械派生**（首条用户消息或用户命名），**无一调用模型** → D6 采用机械派生。

## 3. 三个候选方案

| | **A · 最小复杂度**（Pasteur） | **B · 可扩展优先**（Meitner） | **C · 性能优先**（Euclid） |
|---|---|---|---|
| 新模块 | 1（`resume.rs`） | 2（`resume.rs` + `session_meta.rs`） | 1（`sessions.rs`） |
| 抽象 | 纯函数，无 trait | `SessionCatalog`/`SessionPicker`/`TitlePolicy` 三 trait | 纯函数 |
| `ephemeral` | **删除**（-p 复用持久构造） | 保留 | 保留（测试用） |
| meta 判别 | `type=="meta"` | `type=="meta"` + `serde(flatten) extra` | `$yushan=="meta"` |
| 列表 | 只读首行 + mtime DESC | catalog.list + picker trait | 只读首行 + **有界读取** + mtime DESC |
| append | 全量重写（保住 meta） | 全量重写 | 可选改 O_APPEND |
| CLI | `--resume[=<id>]` + `split_mode_flags` | `parse_resume` 前缀子句 | `--resume [id]` 紧邻槽 |

## 4. 对比

| 维度 | A | B | C |
|---|---|---|---|
| 实现复杂度 | **最低**（1 模块、0 trait） | 高（3 trait + 2 模块） | 低（1 模块） |
| 启动开销 | **零目录扫描**（默认新会话） | 同 A | 同 A，且列有界 |
| 列表开销 | O(N) 次 open+首行 | 同 A | **同 A + 封顶读取**（有界内存） |
| 演进灵活性 | 低 | **高** | 低 |
| 依赖方向 | ✅ | ⚠️ trait 抽象扩散到 app 层 | ✅ |
| 风险 | 低 | **投机抽象**（D11 已排除运行期 `/resume`，多后端/多选择器当前无消费者） | 低（append 优化为可分离项） |

**关键观察**：B 的三个 trait 是为**假想消费者**设计（本仓无第二会话后端、无运行期 `/resume`、
D9 明确不做标题策略配置）。C 的「有界读取 + 只读首行」是**低成本的真实收益**，可直接吸收。

## 5. 最终方案（融合 A + C）

取 **A 的极简结构**（1 个新模块、纯函数、无 trait）
+ **C 的列表实现**（只读首行、封顶读取、mtime DESC）；**弃 B 的三个 trait**。

### 5.1 模块划分

| 位置 | 内容 | 职责 |
|---|---|---|
| `crates/ys-session/src/meta.rs`（新增） | `SessionMeta` | 纯数据；序列化形状 |
| `crates/ys-session/src/lib.rs`（改） | `Session::meta()` / `set_meta()`（**默认实现**：`None` / `Ok(())`） | `MemorySession` 零改动 |
| `crates/ys-session/src/jsonl.rs`（改） | `meta: Option<SessionMeta>`；`flush_to_file` **先写 meta 行再写消息**；`load_from_file` 逐行「Message → SessionMeta → 坏行跳过」 | meta 在 `append`/`clear`/`replace_all` 三条路径天然保住（D7） |
| `apps/coding-agent/src/sessions.rs`（新增） | `list` / `resolve` / `pick` / `derive_title` / `derive_stored_title` | 目录只读视图 + 起屏前选择器；纯同步、无 TUI 依赖 |
| `apps/coding-agent/src/wiring.rs`（改） | `persistent_new` / `resume`；删 `latest_session_path` 与隐式恢复；`meta()` / `update_meta(f)` | 会话生命周期 |
| `apps/coding-agent/src/main.rs`（改） | `parse_args` 增 `--resume[=<id>]` + `split_leading_flags`；起屏前 `resolve`/`pick`；三模式统一持久化；**公共 `finalize_turn_meta`**；`-p`/`--json` 经 `run_single_turn` 调它 | 入口 + 装配 + 三模式写穿 meta |
| `apps/coding-agent/src/app_loop.rs`（改） | 接入公共 `finalize_turn_meta`（首回合标题 + 回合末刷新 model/tokens/thinking；**TUI 路径**） | 写穿 meta |
| `crates/ys-tui-coding/src/view.rs` + `app.rs`（改） | `CodingView.thinking_visible`（**启动初值**）；`App::new` 消费一次 | D10 的 thinking 恢复 |
| `crates/ys-protocol/src/request.rs`（改） | `Request::SetThinking { visible: bool }` | UI → app 回传 thinking 开关 |
| `apps/coding-agent/src/capabilities.rs`（改） | `/compact` 经 `replace_all` 保住 meta（由 `JsonlSession` 保证，无需特判） | — |

**meta 写入点必须三模式共用（review T1）**：`app_loop::run` 是 **TUI 专用**；`-p` / `--json` 走
`run_single_turn`（`apps/coding-agent/src/main.rs:488`），**不经 `app_loop`、也无 `TurnStats`**。
故把「首回合写 title（素材在 skill 展开前）、回合末刷新 model/tokens/thinking」抽成 `main.rs` 的公共函数
`finalize_turn_meta(&mut Wiring, raw_user_text: Option<&str>, usage: Option<&Usage>)`，`app_loop` 与
`run_single_turn` 两条路径都调；`run_print_mode` / `run_json_mode` 必须保留 `RunResult.usage`（现状直接丢弃）。

### 5.2 meta 行：格式与判别

JSONL **首行**为 meta（可缺省），其余行仍是**裸 `Message`**（一字不改）：

```json
{"type":"meta","v":1,"title":"帮我重构 wiring.rs 的会话恢复逻辑","provider":"deepseek","model":"deepseek-chat","thinking_visible":false,"input_tokens":12345,"output_tokens":678,"turns":4,"created_at":1791234567}
```

**判别规则**（`load_from_file` 逐行，与既有「坏行跳过」合流）：

1. 先按 `Message` 解析 —— 成功即消息（绝大多数行走这条）；
2. 失败则按 `SessionMeta` 解析 —— 成功即 meta（`type` 是必填判别键，`Message` 无此键，不会误判）；
3. 两者皆失败 ⇒ 既有行为：告警 + 跳过（不终止）。

`Message` 结构为 `{role, content}`（`crates/ys-core/src/message.rs:13`），无 `type` 字段 ⇒ 判别安全。
`SessionMeta` 用**必填** `type: "meta"` 字段（serde 单变体枚举）保证第 2 步不产生假阳性。

- `title` 存**有界片段**：写入前 `derive_stored_title` 折叠空白/控制字符并 `chars().take(500)`（UTF-8 安全），
  显示层再截到 50 字符 + `…`（D6 修订，review T2）。**不能存原文**：列表只读首行且封顶 4 KiB（C4），
  meta 行超过封顶就会被截断成非法 JSON → 标题静默丢失。500 字符 CJK ≈ 1.5 KiB，加其余字段仍稳在 4 KiB 内。
- `v` 为廉价前瞻；**不读旧格式**（D9）。

### 5.3 SessionMeta 与 Session trait

```rust
// crates/ys-session/src/meta.rs
pub struct SessionMeta {
    pub title: Option<String>,          // 首条真实用户消息原文（D6）
    pub provider: Option<String>,       // D10
    pub model: Option<String>,
    pub thinking_visible: bool,
    pub input_tokens: u32,              // TurnStats 累计
    pub output_tokens: u32,
    pub turns: u32,
    pub created_at: u64,
}

// crates/ys-session/src/lib.rs —— 默认实现让 MemorySession 零改动
pub trait Session {
    fn meta(&self) -> Option<&SessionMeta> { None }
    async fn set_meta(&mut self, meta: SessionMeta) -> Result<(), SessionError> { Ok(()) }
    // …既有 messages / append / clear / replace_all 不变
}
```

```rust
// apps/coding-agent/src/wiring.rs
impl Wiring {
    pub async fn persistent_new(model, events, dir: PathBuf) -> Result<Self, SessionError>;
    pub async fn resume(model, events, dir: PathBuf, path: PathBuf) -> Result<Self, SessionError>;
    pub fn meta(&self) -> Option<&SessionMeta>;
    pub async fn update_meta(&mut self, f: impl FnOnce(&mut SessionMeta)) -> Result<(), SessionError>;
}
```

```rust
// apps/coding-agent/src/sessions.rs —— 纯同步函数，无 trait
pub struct Listing { pub id: String, pub path: PathBuf, pub mtime: SystemTime, pub title: String }
pub fn list(dir: &Path) -> Vec<Listing>;                       // mtime DESC；只读首行（封顶 4 KiB）；不可读文件跳过
pub fn resolve(dir: &Path, id_or_path: &str) -> Result<PathBuf, String>;  // 先 dir/{id}.jsonl，再按路径
pub fn pick(entries: &[Listing]) -> Result<PathBuf, String>;   // 非 TTY ⇒ Err；列表与提示写 stderr
pub fn derive_title(raw: &str) -> String;                      // 显示用：折叠空白 + chars().take(50) + `…`
pub fn derive_stored_title(raw: &str) -> String;               // 落盘用：折叠空白 + chars().take(500)（C6）
```

### 5.4 关键数据流

**A · 默认新会话（无 `--resume`，三模式共通）**

```text
parse_args → Resume::New
  → Wiring::persistent_new(dir)：create_dir_all(dir) 后算出 {unix}_{nanos}.jsonl，
    JsonlSession::open（**惰性，不预建**）
  → 首回合：拿到用户原文（**skill 展开之前**）→ 回合末 finalize_turn_meta 写 title/model/tokens/thinking
  → 此后每回合末刷新；`-p` / `--json` 同一条 finalize_turn_meta（run_single_turn 之后调用）
```

**B · resume 指定 id（`--resume=<id>`）**

```text
sessions::resolve(dir, id) → 命中 dir/{id}.jsonl（或直传路径）；未命中 ⇒ 起屏前 stderr 报错退出
  → Wiring::resume(...) → JsonlSession::open 读回 meta + messages
  → 状态恢复（meta 决定 model，顺序见 5.6）→ 起屏
```

**C · resume 列表选择（`--resume`，无 id）**

```text
sessions::list(dir) → mtime DESC 的 Listing（时间 = mtime；主题 = meta.title 截断，缺省「新会话」）
  **列表为空** ⇒ stderr 提示「没有可恢复的会话」，**走 persistent_new**（等价默认启动，不是报错）
  非 TTY ⇒ stderr 打印列表 + Err("请用 --resume=<id>")（D12）
  TTY  ⇒ sessions::pick 两列（**列表与提示一律写 stderr**）+ 读 stdin → 回到路径 B
```

**picker 的所有输出走 stderr**（review T4）：`-p` 的 stdout 是模型文本、`--json` 的 stdout 是逐行
`Envelope`，任何提示写 stdout 都会污染数据流。

**picker 必须在 `enable_raw_mode`/alt-screen 与 `Wiring` 构造之前**（D12）—— 与现有
「`Wiring` 构造在起屏之前（`rt.block_on`）」同序，天然满足。

### 5.5 CLI 契约

```text
ys-coding-agent [--resume[=<id>]]                          # TUI
ys-coding-agent -p     [--stats] [--resume[=<id>]] <task>  # Print
ys-coding-agent --json [--stats] [--resume[=<id>]] <task>  # JSON
```

- **裸 `--resume`** = 列表选择；**`--resume=<id>`** = 指定会话（`=` 形式**消除与任务文本的歧义**）。
- `parse_args` 的既有规则「只在**首个位置**判 flag」保留；`--stats` 的「紧邻模式 flag 的例外槽」
  泛化为 `split_leading_flags`（同时剥离 `--stats` 与 `--resume[=<id>]`，**限定在 flag 区**，
  不侵入「任务文本逐字保留」）。这是**显式契约变更**，须配解析矩阵测试（见 §6）。
- `split_leading_flags` **循环剥离** flag 区，对 `--stats` / `--resume[=<id>]` **顺序无关**
  （`-p --stats --resume=x "任务"` 与 `-p --resume=x --stats "任务"` 等价）；两者都只认
  「模式 flag 之后、任务文本之前」的位置。
- TUI 的 `--resume[=<id>]` 只在首个位置；其后参数照旧忽略（现状 `_ => Interactive`）。

### 5.6 状态恢复顺序（D10）

`--resume` 命中后，按下列**优先级**解析「用哪个模型」，并在 provider 无凭证时**降级而非失败**：

```text
1. 环境变量完整凭证（config.is_configured()）—— 最高，用户显式覆盖
2. meta 的 provider + model —— 仅当 registry.auth_for(provider) 有凭证
3. state.json 的 last_active_provider/model —— 现状逻辑
4. 第一个存有凭证的 provider —— 现状兜底
```

**`main()` 顺序必须调整（review T3）**：现状 `parse_args` 在 state.json 恢复与 `resolve_limits`
**之后**（`apps/coding-agent/src/main.rs:722` → `:771` → `:792`），而 meta 只有 `parse_args` +
`resolve`/`pick` 之后才拿得到。故顺序改为：

```text
parse_args（含 --resume）→ sessions_dir() → sessions::resolve/pick → 读回 meta
  → 模型解析（上面 1–4 的优先级）→ resolve_limits(&config) → set_model_factory → build_model
```

`-p` / `--json` 同样走这条顺序（场景 5：带 resume 时载入历史与 model）。

- `TurnStats` 用 meta 的 `input_tokens/output_tokens/turns` **播种**（否则 resume 后统计归零）。
- **`thinking_visible` 的单真值机制**：UI 的 `App.thinking_visible` 是运行期真值；app 侧持一份
  **进程级**缓存（resume 时由 `wiring.meta()` 初始化），UI 变化经 `Request::SetThinking { visible }`
  单向同步，回合末落 meta。**`/new` 不重置该缓存**（展示偏好随进程）——否则空 meta 的新会话会让
  UI 与 meta 漂移（review T7）。`CodingView.thinking_visible` 只在启动时被 `App::new` 消费一次，
  此后仅是 app 侧缓存的镜像。
- `session_started`（状态行时长）**不恢复**，resume 后归零（属展示，非执行状态）。
- **`turns` 语义（review T8）**：`meta.turns` = 会话累计回合数，只播种 `TurnStats.turn_count`
  （状态行累计）；`Envelope.turn` 是**本进程**回合序，resume 后仍从 1 起（不续号）。

### 5.7 不变量与正确性论证

- **C1 · meta 永不因重写丢失**：`JsonlSession` 唯一落盘点是 `flush_to_file`，它**先写 meta 再写消息**；
  `append`/`clear`/`replace_all`/`set_meta` 都经它 ⇒ 不存在「漏写 meta」的路径。
  （变异测试：删掉 meta 写分支 → `/compact` 后标题/token 应转红。）
- **C2 · 主题首回合只算一次**：`finalize_turn_meta` 仅在 `meta.title.is_none()` 时写入，
  写入值 = `derive_stored_title(raw)`（折叠 + `chars().take(500)`）⇒ 幂等；
  且因**已落盘**，后续 `/compact` 把首条改写成 `Role::User` 摘要也不会污染标题（D7 的核心理由）。
- **C3 · 标题素材是用户原文**：在 **skill 展开之前**取（`app_loop::expand_skill_invocation` 之前、
  `main::expand_task` 之前）⇒ 不会把 `<skill …>` 信封（9k 正文）当标题。
- **C4 · 列表不解析消息**：`list` 只 `stat` + 读**首行**（封顶 4 KiB）⇒ 与文件大小解耦，
  内存有界；默认启动**零目录扫描**（比现状 `latest_session_path` 每次扫目录更快）。
  该封顶成立的前提是 meta 行自身有界（见 C6）。
- **C5 · 依赖方向不破**：`sessions.rs` 只依赖 `ys-session` + std；`ys-tui-coding` 仍不依赖
  `ys-runtime`；`Request::SetThinking` 只是 `ys-protocol` 的数据变体（无 tokio、无 trait object）。
- **C6 · meta 行有界**：`title` 写入前截断到 500 字符 ⇒ meta 行恒远小于 C4 的 4 KiB 封顶，
  「列表只读首行」不会因超长首条用户消息而失效（review T2；这是 C4 的前提）。
- **C7 · 三模式共用 meta 写入**：title/model/tokens 的唯一写入点是 `finalize_turn_meta`，
  TUI（`app_loop`）与 `-p`/`--json`（`run_single_turn` 之后）都调它 ⇒ D4「三模式统一持久化」
  在**代码落点**上成立，而非只在文档里（review T1）。
- **C8 · picker 不污染数据流**：列表/提示/错误一律写 stderr；空列表走默认新会话而非报错（review T4）。

## 6. 测试计划

| 层 | 用例（对应不变量） |
|---|---|
| `ys-session` | meta 往返；`flush` 后 meta 在首行；`clear`/`replace_all` 后 meta 仍在（C1）；`load` 对「无 meta 的纯消息文件」不告警；坏行仍跳过；**契约快照**：`Message` 的 serde 形状不含 `type`（判别的依赖）；**变异**：删 meta 写分支 → 红 |
| `sessions.rs` | `list` 按 mtime DESC；只读首行且**封顶成立**（超大文件不 OOM，C4）；不可读文件跳过；`resolve` 命中/未命中/先 id 后路径；`derive_title` 折叠空白、截断、空文本默认；**非 TTY ⇒ Err 且输出全在 stderr**（D12/T4）；**空列表 ⇒ 默认新会话** |
| `wiring.rs` | `persistent_new` `create_dir_all` + 不预建文件、不扫目录（D3/T10）；`resume` 读回 meta；`update_meta` 幂等（C2） |
| `main.rs`（`parse_args`） | **解析矩阵**：三模式 × `--resume`/`--resume=id`/`--stats` 组合，**含两种 flag 顺序**（T5）；任务文本逐字保留；未知 flag 报错 |
| `finalize_turn_meta`（`main.rs`/`app_loop`） | 三模式都写 title（skill 展开前，C3）；**`-p`/`--json` 落盘文件有 title + model + tokens**（T1/C7）；长首条消息的标题仍能在列表读回（C6/T2）；回合末刷新；`SetThinking` 落到 meta |
| `ys-tui-coding` | `CodingView.thinking_visible` 初值进 `App`；`/thinking` 变化触发 `Request::SetThinking`；**`/new` 后展示开关不漂移**（T7） |

## 7. 实施步骤

1. `ys-session`：加 `SessionMeta` + trait 默认方法 + `JsonlSession` 首行读写（先写单测，红→绿）
2. `apps/coding-agent/src/sessions.rs`：`list`/`resolve`/`pick`/`derive_title`/`derive_stored_title` + 单测
3. `wiring.rs`：`persistent_new`/`resume`/`update_meta`；删 `latest_session_path` 与隐式恢复
4. `main.rs`：`parse_args` 增 `--resume[=<id>]` + `split_leading_flags` 循环剥离；**把 `parse_args` 与
   起屏前 `resolve`/`pick` 提到模型解析之前**（T3）；公共 `finalize_turn_meta`；`-p`/`--json` 改走
   `persistent_new`/`resume`、保留 `RunResult.usage`、末尾 `eprintln!("session: {id}")`（stderr）
5. `app_loop.rs`：接入公共 `finalize_turn_meta`（首回合标题 + 回合末刷新）
6. `ys-protocol` + `ys-tui-coding`：`Request::SetThinking` + `CodingView.thinking_visible`（启动初值）+
   `/new` 不重置展示开关（T7）
7. 同步文档：`docs/architecture.md` **§10**/§11/§12、`docs/design-final/coding-agent-tui.md`、
   `docs/design.md` §12 行、`wiring.rs` 头注释（删「启动时恢复最近一个」）、`events.rs` 头注释
   （`/thinking` 不再是纯本地动作）
8. `cargo test --workspace` + `cargo clippy --all-targets` + `cargo fmt`

## 8. 被否方案与否决理由

- **B（三个 trait：`SessionCatalog`/`SessionPicker`/`TitlePolicy`）**：否决。当前**无第二消费者**
  （唯一会话后端是本地目录；D11 排除运行期 `/resume`；D9 不做标题策略配置）—— 是**投机抽象**，
  违反「最小核心 / 不做假想消费者的设计」。
- **Euclid 的 `append` 改 O_APPEND 单行**（去掉 tmp+rename 原子性）：**本轮不做**。它改变了
  `JsonlSession` 的原子写契约（现有「写 tmp → rename」），属独立取舍；作为复访项（见 §9）。
- **保留 `latest_session_path` 作为 `--resume` 的「最近」快捷**：否决。D3 明确移除隐式「恢复最近」；
  列表已按 mtime DESC 排序，取首项即最近，无需独立代码路径。
- **`-p`/`--json` 继续用 `ephemeral`**：否决（D4）。但 **`Wiring::ephemeral` 保留**（`MemorySession`
  仍是测试与「不落盘」构造的唯一入口，删除会波及大量测试却无收益）——保留形态见本篇末条
  （标 `#[cfg(test)]`）。
- **meta 用 `$yushan` 判别键**（Euclid）：否决，改用 `type:"meta"`。两者等价，但 `type` 更常规、
  与「首行 typed 条目」的表述一致（Pi 启发）。
- **`serde(flatten) extra` 兜未知字段**（Meitner）：暂缓。前向兼容非当前需求（D9 不背兼容），
  需要时再加（`serde` 支持，成本低）。
- **恢复 thinking 的替代方案（UI/app 各存一份、互为真值）**：否决 —— 两份真值必然漂移。采纳的是
  **UI 单真值 + app 侧进程级缓存（只作持久化镜像）**，由 `Request::SetThinking` 单向同步（§5.6）。
- **`/new` 保留「预建空文件」的 `clear()`**：**删除**。其原始理由（否则 `latest_session_path`
  重启选回旧文件）在 D3 下已失效；保留会写无用空文件且注释变错。相应更新相关用例
  （`new_session_persistent_*`、`restart_after_new_without_messages_recovers_new_empty_session`、
  `recovery_picks_newest_after_new` 等）。
- **`Wiring::ephemeral` 变 test-only**：D4 后生产路径不再用它（`-p`/`--json` 改持久）。**保留但标
  `#[cfg(test)]`**（`MemorySession` 仍是测试「不落盘」构造的唯一入口）；否则非测试构建会报
  `dead_code`（review T11）。
- **`update_meta` 每回合一次额外全量写**：**接受**。回合内已有 N 次 `append` 全量写，+1 次
  ≈ `1/(N+1)` 级开销；不改为「只改内存」，否则「回合末写完就退出」会丢标题。写放大归入 §9 复访项。

## 9. 已知风险与待验证项

- [ ] **`parse_args` 契约放宽**：`split_leading_flags` **循环剥离、顺序无关**，必须配**解析矩阵测试**，
      否则侵蚀「任务文本逐字保留」（本项目有过此类回归）。
- [ ] **`/compact` 丢 meta**：靠 `JsonlSession` 的 C1 保证，须**变异测试**钉死。
- [ ] **model 恢复优先级**（5.6）：需按新的 `main()` 顺序**端到端复现一次**（env > meta > state.json）。
- [ ] **mtime 语义**：全量重写 ⇒ mtime ≈「最后活动」；`/export` 自拷贝护栏、`clear()` 也会动 mtime，
      列表顺序可能略反直觉 —— 文案上说明「最近活动」。
- [ ] **`-p`/`--json` 落盘**：CI/脚本会积空会话（D13 不清理）；故必须在 stderr 打 `session: <id>`。
- [ ] **可发现性**：移除隐式恢复后，启动期给一行 `Resume with --resume` 提示（走既有启动期提示通道）。
- [ ] **无 meta 的老会话**：列表一律显示「新会话」，可达但不可辨识（D9 有意不背兼容）—— 观察是否
      需要按 `mtime` 之外的方式区分。
- [ ] **append 原子性 vs O(n²)**（§8 的复访项）：真出现长会话写放大（含每回合 `update_meta`）再单独设计。

## 参考

- [`context.md`](./context.md) · [`research.md`](./research.md)
- `apps/coding-agent/src/wiring.rs`（`Wiring` / `latest_session_path`）
- `crates/ys-session/src/jsonl.rs`（`flush_to_file` / `load_from_file`）
- `crates/ys-protocol/src/request.rs`（`Request`）
- `crates/ys-tui-coding/src/commands.rs:428`（`/thinking` 本地开关）
- `apps/coding-agent/src/skills.rs:543`（`expand_invocation` 信封形状）
