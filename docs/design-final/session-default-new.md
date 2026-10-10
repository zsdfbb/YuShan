# 会话默认新建 + 显式 `--resume` — As-built

> 性质：**实际建成记录**。决策见 `docs/adr/0026-session-default-new-and-resume.md`，
> 设计见 `docs/arch/session-default-new/design.md`（+ `context.md` / `research.md` / `review.md`），
> 报告见 `docs/reports/2026-10-08-session-default-new-report.md`。
> 日期：2026-10-08 ｜ 基线 `58e0e3b`。

> **已由 ADR-0033 取代（2026-10-10）—— 仅针对「resume 仅启动期一次性」这一点**：
> 本特性立项时的立场是「resume 发生在**启动期一次性**、**不做运行期切换**」（对应 ADR-0026 决策 6 /
> `docs/arch/session-default-new/context.md` 的 **D11**）。ADR-0033 已落地**运行期 `/resume`**
> ⇒ **D11 的后半句「不做运行期切换（无 `/resume` 中途换会话）」作废**。
> **D12 的「列表交互落点 = 起屏前 stdin 选择」未被推翻** —— 它仍是**启动期裸 `--resume`** 的形态；
> 运行期 `/resume` 走的是**另一条通路**（TUI 浮层，经协议 `Request::ListSessions` / `Request::ResumeSession`）。
> 运行期 as-built 见 `docs/design-final/runtime-resume.md`。
> **本文件其余决策（默认新建、meta 首行、三模式统一持久化、`/new` 惰性不预建文件、thinking 单真值、
> 标题派生…）全部保留**；下列正文（含「恢复的三条分支」「恢复优先级」等）描述的是**启动期 `--resume`**
> 的形态，未因运行期 `/resume` 的加入而失效。

## 一句话

交互式启动从「无条件恢复最近会话」改为**默认新会话**；恢复收进显式 `--resume[=<id>]`
（裸 flag = 列表选择，`=<id>` = 指定）。会话状态（title / provider / model / thinking /
tokens / turns）写进**同一 JSONL 文件的首行 meta**（无 sidecar）—— `JsonlSession` 全量重写时
先写 meta 再写消息，故 `/compact` 也不会丢。三模式（TUI / `-p` / `--json`）统一持久化，
共用唯一 meta 写入点 `finalize_turn_meta`。

## 不变量（C1–C8）与实现落点

| # | 不变量 | 落点 |
|---|---|---|
| **C1** | meta 永不因重写丢失 | `crates/ys-session/src/jsonl.rs::flush_to_file`：唯一落盘点，**先写 meta 行、再写消息**；`append`/`clear`/`replace_all`/`set_meta` 全经它 |
| **C2** | 主题只算一次、幂等 | `apps/coding-agent/src/main.rs::finalize_turn_meta`：仅 `meta.title.is_none()` 时写 `sessions::derive_stored_title(raw)` |
| **C3** | 主题素材 = skill 展开前的用户原文 | TUI：`app_loop.rs` 在 `expand_skill_invocation` **之前**取 `sessions::user_text`；`-p`/`--json`：`main::raw_and_expanded_task` 先留底稿 |
| **C4** | 列表不解析消息、默认启动零目录扫描 | `apps/coding-agent/src/sessions.rs::list` 只 `stat` + 读首行；`wiring.rs::persistent_new` 不扫目录 |
| **C5** | 依赖方向不破 | `sessions.rs` 只依赖 `ys-session` + std；`ys-tui-coding` 仍不依赖 `ys-runtime`；`Request::SetThinking` 只是 `ys-protocol` 数据变体 |
| **C6** | meta 行有界 | `sessions::derive_stored_title` 写入前 `chars().take(500)`（UTF-8 安全）⇒ 撑起 C4 的 4 KiB 封顶 |
| **C7** | 三模式共用唯一 meta 写入点 | `main::finalize_turn_meta`；TUI 经 `app_loop`、`-p`/`--json` 经 `persist_single_turn` 调用 |
| **C8** | picker 不污染数据流 | `sessions.rs::pick` / `pick_with` 的列表与提示只写注入的 stderr；空列表回落默认新会话 |

## 落点表

```
crates/ys-session/src/meta.rs（新建）        SessionMeta / MetaKind：JSONL 首行形状
crates/ys-session/src/lib.rs（改）           Session::meta() / set_meta()（默认实现，MemorySession 零改动）；pub use meta::SessionMeta
crates/ys-session/src/jsonl.rs（改）         meta 字段；load_from_file 逐行「Message → SessionMeta → 坏行跳过」；flush_to_file 先写 meta
crates/ys-session/Cargo.toml（改）           + serde（derive）
apps/coding-agent/src/sessions.rs（新建）     list / resolve / pick / read_meta / user_text / derive_title / derive_stored_title
apps/coding-agent/src/wiring.rs（改）         persistent_new / resume / meta / update_meta / new_session（惰性）；删 latest_session_path；ephemeral 标 #[cfg(test)]
apps/coding-agent/src/main.rs（改）           parse_args --resume + split_leading_flags；resolve_resume；四档模型恢复；finalize_turn_meta；session_exit_line（三模式退出的 session: <id> → stderr，ADR-0032）；raw_and_expanded_task
apps/coding-agent/src/app_loop.rs（改）       回合末 finalize_turn_meta；thinking 进程级镜像；SetThinking 臂；/new 后 thinking 不重置；SessionPathHandle（退出提示取号，/new 后刷新）
apps/coding-agent/src/view.rs（改）           build_view 填 CodingView.thinking_visible 初值
apps/coding-agent/src/capabilities.rs（改）   /new 惰性（不再预建空文件）
crates/ys-protocol/src/request.rs（改）       Request::SetThinking { visible: bool }
crates/ys-tui-coding/src/app.rs（改）         App::new 消费 view.thinking_visible 一次；View 快照不覆盖
crates/ys-tui-coding/src/view.rs（改）        CodingView.thinking_visible
crates/ys-tui-coding/src/events.rs（改）      /thinking 本地生效后单向发 Request::SetThinking
```

**未动**：`ys-core`（`Message` 一字未改 —— 「`Message` 无顶层 `type`」是 meta 判别安全的外部依赖）、
`ys-runtime`、`ys-loop`。

## CLI 契约

```text
ys-coding-agent [--resume[=<id>]]                          # TUI
ys-coding-agent -p     [--stats] [--resume[=<id>]] <task>  # Print
ys-coding-agent --json [--stats] [--resume[=<id>]] <task>  # JSON
```

- 裸 `--resume` = 列表选择；`--resume=<id>` = 指定会话（id 或路径）；`--resume <id>`（空格）**不支持**
  （`=` 消除与任务文本的歧义）。
- 模式 flag 之后的 flag 区**顺序无关**：`split_leading_flags` 循环剥离 `--stats` / `--resume[=<id>]`，
  两者都只认「模式 flag 之后、任务文本之前」的位置；任务文本从第一个非 flag 起**逐字保留**。
  重复 flag last-wins 且静默丢弃。

## 启动期顺序（`main()`）

```text
parse_args（含 --resume）→ Config::from_env → load_auth → 用户配置 config.json
  → skill 扫描 → state.json
  → sessions_dir + resolve_resume（resolve/pick + 读回 meta）   ★ 必须在模型解析与建 runtime 之前
  → 模型解析（四档，见下）→ resolve_limits / set_model_factory
  → web 配置 + 启动期清理 → 工具表 + system prompt + 启动期提示
  → 建 runtime → 模式分发
```

- `--resume` 的解析与 picker **必须在建 runtime / 起屏之前**：读回的 meta 参与模型选择；picker 的
  一切输出走 **stderr**（`enable_raw_mode` / alt-screen 之后 stderr 会冲屏）。
- 默认新会话且会话目录非空时，启动期给一行 `Resume with --resume`（走既有启动期提示通道：
  TUI → transcript，`-p`/`--json` → stderr）。

## meta 行：格式与判别

首行为 meta（可缺省），其余行仍是**裸 `Message`**（一字未改）：

```json
{"type":"meta","v":1,"title":"帮我重构 wiring.rs 的会话恢复逻辑","provider":"deepseek","model":"deepseek-chat","thinking_visible":false,"input_tokens":12345,"output_tokens":678,"turns":4,"created_at":1791234567}
```

**判别规则**（`load_from_file` 逐行）：先按 `Message` 解析 → 失败再按 `SessionMeta` 解析 → 皆失败走
既有「坏行跳过 + 告警」。判别安全的前提是 **`Message` 无顶层 `type`** —— 有契约快照用例钉住
（`jsonl.rs::test_message_serde_shape_has_no_type_key`）。

## 恢复优先级（`--resume` 命中后）

```text
1. 环境变量完整凭证（config.is_configured()）—— 最高，用户显式覆盖
2. meta 的 provider + model —— 仅当 registry.auth_for(provider) 有凭证
3. state.json 的 last_active_provider / model
4. 第一个存有凭证的 provider —— 兜底
```

provider 无凭证时**降级而非失败**（不挡起屏）。四档以及「meta.model 为 `null` ⇒ 回落 entry.model」
都由 `tests/session_cli.rs` 的回环 mock 端到端钉住。

## 恢复的三条分支

- `--resume=<id>`：`sessions::resolve(dir, id)` 先试 `dir/{id}.jsonl`、再按字面路径；未命中 ⇒
  stderr 报错 + `exit(1)`。
- 裸 `--resume`：`sessions::list(dir)`（**mtime DESC**；只读首行、封顶 4 KiB）→
  - **空列表** ⇒ stderr 提示「没有可恢复的会话」+ **回落默认新会话**（不是报错）；
  - **非 TTY** ⇒ stderr 打印列表 + 一行「非交互环境：请用 `--resume=<id>`」+ `Err`（起屏前 `exit(1)`）；
  - **TTY** ⇒ `sessions::pick` 两列（编号 / 时间 / 标题 / id）读 stdin；列表与提示一律 stderr。

`list` 的读法：字节级读取 + 换行切分 + **最后才解码**（流式/字节路径铁律）；不可读 / 首行超 4 KiB
封顶 / `.jsonl.tmp` 残留一律跳过；标题取 `meta.title` 截断，缺省「新会话」。

## meta 写入时机：三模式共用 `finalize_turn_meta`

- title/provider/model/tokens 的唯一写入点（C7）：TUI 经 `app_loop` 回合末调用，`-p`/`--json` 经
  `persist_single_turn` 调用。
- `provider`/`model` **仅在 `Some` 时写**（`None` 不清空已落盘值）—— 否则 env 凭证路径下
  `config.provider` 恒为 `None`，会把 `/login` 写好的 `provider` 覆盖成 `null`，使恢复优先级第 2 档
  永久失效。
- `thinking_visible` 仅 `Some` 时写（`-p`/`--json` 不参与 thinking 开关）；tokens / turns 每次刷新。
- `update_meta` 每回合一次**全量重写**（写放大，接受，见复访项）。

## `/new` 惰性

`/new` 只换 `session` 句柄（旧文件保留），**不预建空文件** —— 新文件首次 `append` 才落盘。原「预建」
的理由（否则 `latest_session_path` 重启选回旧文件）在「不再隐式恢复最近」后已失效。

## thinking 单真值

- **UI 真值**：`App.thinking_visible`（`/thinking` 本地即时生效）。
- **app 进程级镜像**：启动时由 `wiring.meta().thinking_visible` 初始化；`Request::SetThinking { visible }`
  **单向**同步；回合末落 meta。
- `CodingView.thinking_visible` 只在启动时被 `App::new` 消费一次；运行期 `Outbound::View` 快照
  **不覆盖**它。
- **`/new` 不重置**该镜像（展示偏好随进程）—— 否则空 meta 的新会话会让 UI 与 meta 漂移（review T7）。

## 实现与设计的偏差 / review 后修掉的缺陷

1. **`finalize_turn_meta` 曾无条件写 provider**（真缺陷）：env 凭证路径下 `config.provider` 恒为
   `None`，无条件写会把 `/login` 落盘的 `provider` 清成 `null` ⇒ 恢复优先级第 2 档
   （`meta.provider` + `registry.auth_for`）永久失效。已改为 `provider`/`model`/`thinking_visible`
   **仅 `Some` 时写**（回归用例 `finalize_turn_meta_keeps_provider_model_when_none`）。
2. **`tests/session_cli.rs` 首轮 6 例未真正钉住优先级链**（真缺陷）：最上档用例缺 `--resume`、
   次档用例缺 state.json，「env > state.json」的对照不成立。已补齐 + 独立变异复核（改坏优先级即红）。

## 测试资产

| 位置 | 覆盖 |
|---|---|
| `crates/ys-session/src/jsonl.rs` + `meta.rs` 内联 | meta 往返；meta 在首行；`clear` 与 `replace_all` 后 meta 仍在；纯消息文件无 meta 不告警；坏行跳过；`Message` serde 形状不含 `type` 的快照 |
| `apps/coding-agent/src/sessions.rs` 内联（12 例） | `list` mtime DESC / 只读首行与 4 KiB 封顶 / 不可读与 `.tmp` 跳过 / `resolve` 命中与未命中 / `pick` 非 TTY 注入式写者断言 / `derive_title` 与 `derive_stored_title` |
| `apps/coding-agent/src/main.rs` 内联 | `parse_args` 解析矩阵（三模式 × `--resume`/`--resume=id`/`--stats`，含两种 flag 顺序）；`finalize_turn_meta_*`（title 幂等 + provider/model 不被清空）；`persist_single_turn_*`；`raw_and_expanded_task_*` |
| `apps/coding-agent/src/app_loop.rs` 内联 | `title_uses_pre_expansion_user_text`（C3）；`thinking_cache_survives_new_session`（T7）；`every_request_variant_is_handled` 的 `SetThinking` 臂 |
| `apps/coding-agent/tests/session_cli.rs`（新增 9 例，子进程 + 无凭证 + 回环 mock） | 非 TTY picker 不污染 stdout（退出码 ≠ 0 / stdout 空 / stderr 含表头+标题+id+非交互提示）；空列表回落（`没有可恢复的会话` 先于 `No model configured`）；模型恢复优先级端到端（读真实 HTTP 请求体的 `model` 字段）：env 胜出 → resume meta 胜出 → meta.model 为 `null` 回落 entry.model → meta.provider 无凭证降级 state.json → state.json 胜出，五例均断言路径含 `/chat/completions`；成功回合（回真实 SSE 的回环 mock，2 例）：`-p` / `--json` 的 stdout 保持纯数据流、`session: <id>` 走 stderr 且与落盘 stem 一致、落盘首行 meta 有 title / model / turns |
| `crates/ys-tui-coding` | `App::new` 消费 `CodingView.thinking_visible` 初值；`Outbound::View` 快照不覆盖 thinking；`/thinking` 变化触发 `Request::SetThinking` |
| `crates/ys-protocol` | `Request::SetThinking` roundtrip |

**验证**：`cargo test --workspace` → **896 passed / 0 failed / 7 ignored**；
`cargo clippy --all-targets` → **0 warning**；本特性改动文件 `rustfmt --check` 干净。

## 已知风险 / 复访项

- **`SessionMeta::default()` 得 `v = 0`**：`v` 的 `#[serde(default = "default_version")]` 只在反序列化
  缺字段时补 1；`Default::default()` 构造的 struct 其 `v` 仍是 `0`。构造应走 `SessionMeta::new(created_at)`。
- **`MetaKind` 未 `pub use`**：只导出 `SessionMeta`；外部无法显式表达判别键类型（当前无消费者）。
- **`provider` / `model` 可能不成对**：逐字段「仅 `Some` 时写」⇒ env provider 为 `None` 时会保留旧
  `provider` 而刷新 `model`，出现 provider/model 不匹配（对恢复优先级无害，但读 meta 时需知情）。
- **meta 仅在成功回合写**：首回合失败 ⇒ 无 title、脚本拿不到 `session: <id>`
  （`persist_single_turn` 在 `run_single_turn` 之后）。TUI 侧同理：**没落盘就不打**
  （`session_exit_line` 的 `is_file` 守卫，ADR-0032）。
- **`update_meta` 每回合全量重写**：写放大 ≈ `1/(N+1)` 级；长会话下与 `append` 的 O(n²) 都属既有
  `JsonlSession` 策略。
- **多进程并发写同一文件无锁**：整文件重写 + 固定 tmp 名会丢更新（既有缺口，未改）。
- **空会话不清理**：`-p`/`--json` 与 `/new` 会积 `.jsonl`（清理归范围外）。
- **mtime ≈ 最后活动**：全量重写 + `/export` 自拷贝护栏 + `clear()` 都会动 mtime，列表顺序可能略反直觉。
- **无 meta 的老会话**：列表一律显示「新会话」，可达但不可辨识（D9 有意不背兼容）。
- **测试钉子缺口**：`with_resume_hint` 只钉纯函数未钉调用点；`session: <id>` 用 `contains` 匹配；组 4 `turns >= 1` 偏弱且未核对 `Envelope` 字段；组 3 / 组 4 mock 脚手架重复约 130 行。（详见修改报告「已知后续项」）

## 参考

- `docs/adr/0026-session-default-new-and-resume.md`
- `docs/arch/session-default-new/{context,design,research,review}.md`
- `docs/reports/2026-10-08-session-default-new-report.md`
- `docs/architecture.md` §10–§12
