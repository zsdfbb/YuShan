# 会话默认新建（session-default-new）— 修改报告

> 日期：2026-10-08 ｜ 编排：sequential-workflow **quick** 方案（跳过 Phase 2，无 design-plan / exec-plan 存档）｜ 决策：`docs/adr/0026-session-default-new-and-resume.md`
> 基线：`58e0e3b9`（`baseline.sh` 建立的基线 commit；其前的原始 HEAD = `f95736c`；另存备份分支 `sw-backup/2026-10-07-session-default-new`）
> 设计来源：`docs/arch/session-default-new/design.md`（+ `context.md` 决策 D1–D13 / `research.md` 业界对照 / `review.md`）

## 结果

| 指标 | 基线 | 完成后 |
|---|---|---|
| `cargo test --workspace` | 833 passed / 7 ignored（AGENTS.md 原记） | **896 passed / 0 failed / 7 ignored** |
| `cargo clippy --all-targets` | 0 warning | **0 warning** |
| `cargo fmt`（本特性改动文件） | — | `rustfmt --check` 干净 |
| 改动面 | — | **13 改 + 3 新**，无计划外改动 |

`cargo fmt --all --check` 另报 6 个文件，但它们在**基线上即为 fmt-dirty**，不属本切片：
`config.rs`、`prompt.rs`、`skills.rs`、`tests/e2e_skills.rs`、`tests/truncation_guard.rs`、
`crates/ys-loop/src/basic.rs` —— 已用 `git show 58e0e3b:<f>` + `rustfmt --check` 逐一验证。
AGENTS.md 原记的 `833 / 7` 已随本切片同步为 `896 / 7`。

## 要解决的问题

**旧行为**

- 交互式 TUI 启动**无条件恢复最近会话**：`Wiring::persistent` → `latest_session_path` 取
  `{unix秒}_{纳秒}.jsonl` 里字典序最大者 —— 用户无法「开一个新的」。
- `-p` / `--json` 用 `MemorySession`（`Wiring::ephemeral`）**不落盘、无标题**，脚本无从找回会话。
- `/new` 为避免「重启又选回旧文件」而**预建空文件** —— 写无用空文件，且注释里的理由随隐式恢复一并失效。

**新行为**

- **默认新会话**：零目录扫描、惰性不预建。
- 恢复只走显式 `--resume[=<id>]`：裸 flag = 列表选择，`=<id>` = 指定 id 或路径。
- 三模式（TUI / `-p` / `--json`）统一持久化，并在 JSONL **首行写 meta**。

## 交付内容

**新增**

- `crates/ys-session/src/meta.rs` — `SessionMeta` / `MetaKind`；`type` 为**必填判别键**。
- `apps/coding-agent/src/sessions.rs` — `list` / `resolve` / `pick` / `derive_title` / `derive_stored_title` /
  `read_meta` / `user_text`；纯同步、无 TUI 依赖。
- `apps/coding-agent/tests/session_cli.rs` — 子进程端到端 9 例：非 TTY picker 的 stdout 纯净 / 空列表回落 /
  **模型恢复优先级四档**（用回环 mock 抓真实请求体）；成功回合（回真实 SSE 的回环 mock）：`-p` / `--json` 落盘 meta + `session: <id>` 走 stderr。

**修改**

- `crates/ys-session/{lib.rs,jsonl.rs,Cargo.toml}` — trait 默认 `meta()` / `set_meta()`；
  `flush_to_file` **先写 meta**、`load_from_file` 逐行「Message → SessionMeta → 坏行跳过」；+ `serde` derive。
- `crates/ys-protocol/src/request.rs` — 新增 `Request::SetThinking { visible: bool }`。
- `crates/ys-tui-coding/{view.rs,app.rs,events.rs}` — `CodingView.thinking_visible` 启动初值；
  `App::new` 消费一次；`/thinking` 本地生效后**单向**回传 `SetThinking`。
- `apps/coding-agent/wiring.rs` — 删 `persistent` / `latest_session_path`；新增
  `persistent_new` / `resume` / `meta` / `update_meta`；`ephemeral` 标 `#[cfg(test)]`；`/new` 去预建。
- `apps/coding-agent/main.rs` — `Resume{New,Pick,Id}` + `split_leading_flags` 循环剥离、顺序无关；
  起屏前 `resolve` / `pick`；三模式统一持久化 + `session: <id>` → **stderr**；公共 `finalize_turn_meta`；
  模型恢复优先级链。
- `apps/coding-agent/app_loop.rs` — 进程级 thinking 缓存；标题素材取 skill 展开**前**原文；回合末写 meta。
- `apps/coding-agent/{view.rs,capabilities.rs}` — 初值接线；`/compact` 经 `replace_all` 保住 meta。

## 关键决策与理由

| 决策 | 理由 |
|---|---|
| 默认新会话 + 显式 `--resume`（D1 / D3） | 启动不猜测用户意图；恢复成为显式动作 |
| 默认启动**零目录扫描** | 比「每次启动先扫目录找最新」更快，且与「不恢复」语义自洽 |
| meta 放 JSONL **首行**、`type:"meta"` 作判别键 | `Message` 无 `type` ⇒ 判别安全（有 serde 形状**快照**用例钉住） |
| jsonl 写路径唯一化 | `append` / `clear` / `replace_all` 全经唯一落盘点 ⇒ meta 不丢（C1） |
| title **有界**：落盘截 500 字符（CJK ≈ 1.5 KiB） | 列表**只读首行**且封顶 4 KiB ⇒ 列表读数与文件大小解耦、内存有界（C4 / C6） |
| title 素材取 **skill 展开前**的用户原文（C3） | 否则 `<skill …>` 信封的 9k 正文会变成标题 |
| title 仅缺省时写（C2 幂等） | `/compact` 改写首条消息也不会污染标题（D7 的核心理由） |
| meta 写入点三模式共用 `finalize_turn_meta`（C7） | `app_loop` 是 TUI 专用，`-p` / `--json` 另有 `run_single_turn` 路径，故抽成公共函数 |
| picker 输出**全走 stderr**（C8 / T4） | `-p` stdout 是模型文本、`--json` stdout 是逐行 Envelope，不能被选择器污染 |
| 空列表**回落**新会话而非报错（D12） | 「没有可恢复的会话」不是错误，直接开新会话即可 |
| `provider` / `model` / `thinking_visible` **仅 `Some` 时写** | `None` 不清空已有值 —— 见「真缺陷 ①」 |
| thinking **单真值**：`App.thinking_visible` 是运行期真值，app 侧持**进程级镜像**，经 `SetThinking` 单向同步 | `/new` **不重置**该缓存（T7）：否则空 meta 的新会话会让 UI 与 meta 漂移 |
| `Wiring::ephemeral` 保留但 `#[cfg(test)]` | `MemorySession` 仍是测试「不落盘」构造的唯一入口，删它会波及大量测试 |
| 弃 B 方案三个 trait（`SessionCatalog` / `SessionPicker` / `TitlePolicy`） | 当前无第二消费者，属投机抽象；吸收 C 方案的「只读首行 + 封顶」 |
| `O_APPEND` 优化 | 列为复访项（会改原子写契约），本次不做 |

## 评审中发现并修掉的真缺陷

本节是本报告的重点。两条均为「测试全绿时 bug 依然存在」那一类，靠独立 review + 变异复现抓出。

### 缺陷 ①：`finalize_turn_meta` 无条件写 `provider` / `model`，把 `meta.provider` 清成 `null`

- **现象**：U3 review 的 P2 中唯一功能性项（U4 修复）。env 凭证路径下 `config.provider` 恒为 `None`
  （`Config::from_env` 里 `provider: None`），而 `-p` / `--json` 现在也持久化 ⇒ 用户先用 `/login` 写好
  `meta.provider="deepseek"`，之后任何一次 `-p` 回合末都会被**覆写成 `null`** ⇒ 恢复优先级第 2 档
  （`meta.provider` + `registry.auth_for`）**永久失效**。
- **根因**：写 meta 时把「当前 config 的 `None`」当成「要清空」，未区分「未知」与「空」。
- **修法**：改为 `if let Some(...)` 才写，`None` 保留旧值。
- **变异证据**：把赋值改回无条件 ⇒ 目标用例 `finalize_turn_meta_keeps_provider_model_when_none` 变红。

### 缺陷 ②：`tests/session_cli.rs` 首轮 6 例没有真正钉住「恢复优先级链」

- **现象**：U5 首轮独立 review 判**不通过**，第 2 轮修补 + 复核通过。两个「名字声称的验证点」并未落到断言上：
  - **P1-a**：`model_recovery_env_wins_over_resume_meta_and_auth` 的命令**没有 `--resume`**
    ⇒ `resume_meta` 恒为 `None`，「env > resume meta」从未被验证。变异「把 meta 恢复提到 env 之前」
    在首轮 **6 例全绿**。
  - **P1-b**：`model_recovery_resume_meta_wins_over_state_and_entry` 的 setup **没写 `state.json`**
    ⇒ 「meta > state.json」从未被验证。变异「让 state 覆盖 meta」在首轮 **6 例全绿**。
- **根因**：用例按「预期优先级」命名，但没有把对照档位真的放进 fixture；缺失的档位使断言恒真（空转）。
- **修法**：C1 加 `--resume=s1`（meta 与 env 并存，断言 env 胜、且 meta / auth 的 model 未发出）；
  C2 写 `state.json`（断言 meta 胜、state 与 entry 的 model 都未发出）；另补
  「`meta.provider` 无 auth entry 时**降级**到 `state.json`」一例；全部组 3 用例断言路径含
  `/chat/completions`，`model_sent` 改为解析 body JSON。
- **复核（第 3 个 agent，副本内独立变异）**：M-a（去掉 env 短路）⇒ C1 红；M-b（`from_meta` 强制 `None`）
  ⇒ C2 红；M-c（`meta.provider` 无凭证时直接失败而非降级）⇒ 新用例红；均**只红目标用例**，
  9 例连跑 3 次无 flaky。
- **一句话教训**：**「名字声称的验证点」必须逐个核到断言上** —— 两条 setup 缺失让两个真实回归在绿测下存活。

## 变异证据汇总

| # | 变异 | 目标用例 | 红 / 绿 | 由谁做 |
|---|---|---|---|---|
| 1 | `derive_stored_title` 去掉 `take(500)` | title 有界用例 | 红 | U3 review（Anscombe） |
| 2 | 去掉首行 4 KiB 封顶 | 首行封顶用例 | 红 | U3 review（Anscombe） |
| 3 | 标题素材移到 skill 展开**之后** | 标题素材用例 | **绿（漏检）** | U3 review（Anscombe） |
| 4 | `raw_task` 取展开后文本 | 标题素材用例 | **绿（漏检）** | U3 review（Anscombe） |
| 5 | `pick` 改 stdout | picker stdout 用例 | **绿（漏检）** | U3 review（Anscombe） |
| 6 | `provider` 改回无条件写 | `…keeps_provider_model_when_none` | 红 | U4（Gibbs 复核） |
| 7 | 标题素材移到展开后 | 标题素材用例 | 红 | U4（Gibbs 复核） |
| 8 | `raw` 取展开后 | 标题素材用例 | 红 | U4（Gibbs 复核） |
| 9 | `pick_with` 改 `println!` | picker stdout 用例 | 红 | U4（Gibbs 复核） |
| 10 | M1 恢复分支置于 env 短路前 | C1 | 红 | U5 首轮（Schrodinger） |
| 11 | M2 `pick` 写 stdout | 组 1 | 红 | U5 首轮（Schrodinger） |
| 12 | M3 空列表改成 `exit(1)` | 组 2 | 红 | U5 首轮（Schrodinger） |
| 13 | M4 state 覆盖 meta | C2 | **绿（P1）** | U5 首轮（Schrodinger） |
| 14 | M5 meta 覆盖 env | C1 | **绿（P1）** | U5 首轮（Schrodinger） |
| 15 | M-a 去掉 env 短路 | C1 | 红 | U5 第 2 轮 + 复核 |
| 16 | M-b `from_meta` 强制 `None` | C2 | 红 | U5 第 2 轮 + 复核 |
| 17 | M-c `meta.provider` 无凭证时直接失败 | 降级用例 | 红 | U5 第 2 轮 + 复核 |
| 18 | 删 `flush_to_file` 的 meta 写分支 | `test_meta_roundtrip` / `test_meta_is_first_line` / `test_meta_survives_clear_and_replace_all` | 红（3 条） | Phase 5 独立复核 |

U4 对前三条漏检补齐后为 **4/4 捕获**；U5 第 2 轮的 M-a / M-b / M-c 均只红目标用例。

第 18 条为 Phase 5 补做的 C1 合同证据（设计 §6 测试计划第一行要求「删 meta 写分支 ⇒ 应转红」，
但此前未进入证据表）。结果：3 条 meta 用例变红；其中 `test_meta_survives_clear_and_replace_all`
的内存侧三条断言（`clear` / `replace_all` 后）**仍绿** —— 该用例的杀伤力全部落在「重新打开文件」
那条落盘断言上，正是 `/compact` 重写路径。变异已完整回滚（`jsonl.rs` 哈希与 `git diff 58e0e3b9` 哈希前后一致）。

## 其它评审结论

- **U1 `ys-session`（Hume）通过**：P0 / P1 = 0。P2：`SessionMeta::default()` 得 `v=0`
  （构造路径须用 `new()`）、`MetaKind` 未 `pub use`、`default_version` 未覆盖。
- **U2 `ys-protocol` + `ys-tui-coding`（Lorentz）通过**：P0 / P1 = 0。P2：`commands.rs` 注释
  「7 个变体」过期（实为 8）、`SetThinking` 无 wire-shape 快照。
- **U3 `apps/coding-agent`（Anscombe）通过**：P0 / P1 = 0。P2 = 上面两条真缺陷 + 3 条测试钉子缺失。
- **U4 收尾（Gibbs）通过**：4/4 变异捕获。P2：`provider` / `model` 可能**不成对**（env 下 provider 为
  `None` ⇒ 保留旧 provider 却刷新 model，后续第 2 档会用旧 provider 的 auth + env 的 model）；
  meta 仅在**成功**回合写（首回合失败 ⇒ 无 title、脚本拿不到 `session: <id>`）。
- **U5 `tests/session_cli.rs`（Schrodinger → 复核 Franklin）通过**。
- **U6 Phase 5 补钉子（Descartes 实现 → Avicenna 复核）通过**：新增 `parse_resume_matrix_and_flag_order`（三模式 × `--resume` / `--resume=id` / `--stats`，含两种 flag 顺序等价）与 `with_resume_hint_only_appends_when_triggered`（未触发原样返回 / 触发时追加且原提示在前）；变异均只红目标用例。P2 见下节前四条。

U1 / U2 / U3 / U4 的 P2 统一归入「已知后续项」。

## 已知后续项（本切片范围外，未做）

- `with_resume_hint` 只钉了纯函数，未钉 `main.rs` 的调用点：删掉调用点全部测试仍绿 —— 需子进程级断言（会话目录非空 + 默认新会话 ⇒ 提示确实出现在 stderr / transcript）。
- 组 4 的 `session: <id>` 断言用 `contains` 匹配：同行尾随内容仍绿（U6 复核变异实证）—— 宜整行匹配。
- 组 4 的 `turns >= 1` 对单回合偏弱（宜 `== 1`）；`--json` 成功用例只验证每行是合法 JSON，未逐字段核对 `Envelope` 内容。
- 组 3 / 组 4 两套回环 mock 脚手架重复约 130 行（`serve_once` / `serve_sse_once`）—— 可抽公共 listener。
- meta 每次 `update_meta` 全量重写（写放大，`§9` 复访项）——可评估增量落盘。
- `SessionMeta::default()` 的 `v=0` ——构造路径统一走 `new()`，或给 `default_version` 加覆盖。
- `MetaKind` 未随 `SessionMeta` 一起 `pub use` ——补导出。
- `provider` / `model` 不成对（见 U4 P2）——考虑成对写入或显式成对清理。
- meta 仅在成功回合写（首回合失败 ⇒ 无 title / 无 `session: <id>`）——考虑失败回合也落最小 meta。
- `session_cli.rs` 里三处**否定断言空转**：C1 的 `!meta-model` / `!auth-model`（meta / auth 的 api_base
  指向不可达端口，listener 收不到请求故恒真）；C2b 未隔离「meta.model=null 回落」；C2c 的
  `!ghost-model` 冗余 —— 补可达端口或改为正面断言。
- 多进程并发写同一会话文件无锁（既有缺口，整文件重写 + 固定 tmp 名）。
- 空会话不清理（D13 有意）；`/compact` 之外的会话文件操作会不会丢 meta 已由 C1 覆盖。
- `docs/design-final/core-channel.md` 的历史小节已加作废指针（本次）。

## 波及文档

- `docs/adr/0026-session-default-new-and-resume.md` ✅
- `docs/architecture.md` §10 / §11 / §12 / §15 ✅
- `docs/design.md` §12 测试矩阵 ✅
- `docs/design-final/session-default-new.md`（新，as-built）✅
- `docs/design-final/core-channel.md`（作废指针）✅ / `docs/design-final/coding-agent-tui.md`（表补行）✅
- `AGENTS.md`（测试计数 896/7 + `--resume` 命令 + as-built 列表）✅
- `docs/design-plans/` / `docs/exec-plans/` —— ❌ quick 方案跳过 Phase 2，无存档
- 归档：quick 方案**无** design-plan / exec-plan ⇒ 无 `archive.sh` 动作

## 环境说明

`tests/session_cli.rs`、`stream_usage_fallback.rs`、`truncation_guard.rs` 需要**回环 bind**；
默认沙箱会拒绝（`Operation not permitted`），需在**无沙箱环境**跑（本次已在沙箱外验证全绿）。
工作区自身无法建立沙箱内的回环 mock，属环境限制而非测试缺陷。

## 版本控制提示

基线 commit `58e0e3b9` 是 `baseline.sh` 为本次改动建立的（工作区原本不干净，另存备份分支
`sw-backup/2026-10-07-session-default-new`）。若不需要保留该基线 commit，可用
`git reset --soft f95736c` 消掉（**由用户决定，本次未执行**）。
