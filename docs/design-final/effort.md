# `/effort` 思考档位 —— as-built

`/effort` 在会话内覆盖模型的思考强度。规范取值由 `ys-core::EffortLevel` 唯一给出；
请求实际发送的字段仍是 provider 的 `reasoning_effort`，能力门控与默认值沿用
ADR-0042（默认 `high`，仅 `supports_reasoning_effort` 的 provider 发送）。

## 1. 行为契约

| 输入 | TUI 行为 | 发给 app | 输出 |
|---|---|---|---|
| `/effort` | 本地读 `CodingView.effort`，写**恰好一条** System 行（有效档位 + 用法），**不发请求** | — | `effort: <level>`（有效值），附用法串 |
| `/effort <level>` | 解析为七个规范档位之一后直发 | `SetEffort { effort: Some(level) }` | `effort: <level>` |
| `/effort default` | 大小写不敏感地识别清除令牌 | `SetEffort { effort: None }` | `effort: <配置默认>`（默认 `high`） |
| `/effort <非法值>` | 本地用法提示，**不改变任何状态**、不发请求 | — | `用法：/effort [off|minimal|low|medium|high|xhigh|max|default]` |

- 合法档位：`off` / `minimal` / `low` / `medium` / `high` / `xhigh` / `max`。
  解析前 `trim`、大小写不敏感；**`default` 不是档位**，而是 `Option<EffortLevel>` 的
  `None`（清除会话覆盖，回落到配置默认 `YUSHAN_REASONING_EFFORT`，默认 `high`）。
- `xhigh` 是规范拼写（无连字符）。`EffortLevel::as_wire` 与 serde 的
  `rename_all = "lowercase"` 是同一契约的两种呈现。
- 无参显示**只读视图**：TUI 不持有第二份档位真值，不先于 app 改写本地状态。
- `/status` 固定含一行 `effort: <有效档位>`（规范拼写，非 `Debug` 名）。
- 设置后**下一回合**生效：回合使用回合开始时取定的模型实例，回合中不热切换。

## 2. 数据流

```
TUI 输入
  └─ commands::parse → Action::Request(Request::SetEffort { effort })
       └─ app_loop（信道 ①）→ match Request::SetEffort
            └─ capabilities::set_effort(&mut Config, &mut Wiring, effort)
                 ├─ config.effort = effort          // None = 清除会话覆盖
                 ├─ config.build_model() → wiring.set_model(Some | None)
                 └─ Vec<String> → Outbound::Output；随后 Outbound::View（有效档位）
                      └─ 下一回合：model factory 读
                         cfg.effective_reasoning_effort()
                           ├─ 会话覆盖（config.effort）优先；
                           └─ 否则回落配置默认（YUSHAN_REASONING_EFFORT / high）
                         → reasoning_effort_for(compat, effective)
                              ├─ supports_reasoning_effort = true ⇒ 字段进请求
                              └─ false ⇒ 不发送（命令层给 warning）
                      └─ 回合末：finalize_turn_meta(..., Some(config.effort), ...)
                           └─ SessionMeta.effort（只存**覆盖值**；TUI 模式）
```

- **运行期真值**：有效强度 = `Config.effort`（会话覆盖）或
  `Config.reasoning_effort`（配置默认）。`Wiring.model` 是当前回合实际使用的模型实例。
- 有效强度落在 `OpenAICompatibleConfig.reasoning_effort` 上，故只有重建 model 才会
  进入请求。
- 模型工厂（主模型与 `build_model_for`）都读 `effective_reasoning_effort()`，因此
  subagent、压缩 / 摘要等经工厂构造的调用**自动继承**当前有效档位。
- 重建失败（缺凭证）⇒ `wiring.set_model(None)`，与 `/model` 同一契约：绝不留下
  与 `Config` 不一致的旧模型实例。
- `capabilities.rs` 不 print：所有用户可见行经 `Vec<String>` → `Outbound::Output`
  → transcript。
- 启动期 `--resume` 与运行期 `/resume` 都从目标会话 meta 读**覆盖值**：
  `build_candidate_model` 用「候选四字段 + 候选覆盖」构建成功后，提交步才写回
  `cfg.effort`；失败路径不改动当前配置。

### 子 Agent 与辅助调用

- 主模型的 `Config::build_model` 与按名构造子 Agent 的 `Config::build_model_for`
  都读有效强度；未指定模型名的子 Agent 直接复用父模型实例，因此同样继承。
- 具名子 Agent 的模型实例缓存键是 `(模型名, 有效推理强度)`：`/effort` 或配置默认
  变化后，同名派发会以新档位重建实例，不会命中旧档位的实例。缓存跨派发复用连接池
  与 TLS。
- 已知边界：缓存键不含 provider / api_base，二者变化不会使同名子 Agent 的缓存
  实例失效（既有局限，不在 `/effort` 的范围内）。

## 3. provider 能力门控（ADR-0042）

- `ProviderCompat.supports_reasoning_effort` 决定字段是否进入请求体。
  当前仅 `opencode-go` 为 `true`；`deepseek` / `minimax` / custom / 未知端点默认
  `false`，避免严格端点因未知顶层字段返回 400。
- 适配器只做一件事：`OpenAICompatibleConfig.reasoning_effort` 原样序列化为顶层
  `reasoning_effort`；`None` 时整个字段不序列化。
- 档位 wire 拼写：`off|minimal|low|medium|high|xhigh|max`；服务端折叠（例如把
  `medium` 当作 `high`）由 provider 负责，适配器不做本地映射。
- 不支持的 provider：设置**照常保存**；命令层输出一条 warning，说明本次请求不会
  发送思考字段。切到支持的 provider 后立即生效。

## 4. 持久化语义

- 落点：`ys-session::SessionMeta.effort: Option<EffortLevel>`，JSONL 首行。
  `#[serde(default)]` 是回写兼容契约：缺该键的旧首行反序列化为 `None`，显式
  `null` 同样为 `None`。**只存会话覆盖值**，不存配置默认。
- **TUI 模式**：回合末由 `finalize_turn_meta` 写穿。参数是三态：
  - `Some(Some(level))` ⇒ 写入覆盖；
  - `Some(None)` ⇒ 清除覆盖；
  - `None` ⇒ 不写（`-p` / `--json`）。
  因此单独执行 `/effort` 而不跑回合不会落盘；下一次成功回合才写入。
- **`/new` 不重置**：只换会话句柄，`config.effort` 保持；新会话首个回合末把当前
  覆盖写入新 meta（与 `thinking_visible` 同规）。
- **`/resume` 还原**：无参列表选择与有参直切都读目标 meta 的 `effort`，`None`
  即清除当前覆盖，并在提交前用候选覆盖构建模型。
- **启动期 `--resume`**：`config.effort = resume_meta.effort`；无 resume meta 时保持
  `None` ⇒ 使用配置默认（`YUSHAN_REASONING_EFFORT` / `high`）。
- **`-p` / `--json` 不写**：两者没有 `/effort` 命令，收尾时向 `finalize_turn_meta`
  传外层 `None`，不覆盖既有会话 meta。若启动时 `--resume` 读到覆盖，则本次请求按
  该覆盖发送。
- **不进 `config.json`**：`Config.effort` 不是用户配置项，只由运行期命令 / 会话
  meta 赋值；配置默认仍由 `YUSHAN_REASONING_EFFORT` 给出。

## 5. 关键不变量

- `ys-core::EffortLevel` 无 tokio、无 provider 知识；它是请求侧档位拼写的唯一来源。
- `EffortLevel::from_str` **严格**接受七个规范拼写，拒绝 `default` 与未知值；错误
  保留原始输入并列出全部合法值。
- 有效强度 = 会话覆盖 > 配置默认；设置后必须重建 `Wiring.model`，否则请求不会改变。
- `supports_reasoning_effort = false` 的 provider **绝不**收到 `reasoning_effort`
  字段；命令层只给 warning，不改请求。
- `set_effort` 在缺凭证时清空 wiring model，绝不保留与 config 不一致的旧实例。
- `capabilities.rs` 不 print；命令期输出只走 `Outbound::Output`。
- 旧 JSONL（无 `effort` 键）必须仍可反序列化。

## 6. 测试覆盖与变异点

| 层 | 用例 | 变异（注入 ⇒ 预期变红） |
|---|---|---|
| `ys-core`（`effort.rs`） | `test_all_levels_parse_and_display_roundtrip` / `test_serde_json_roundtrip` / `test_invalid_values_rejected` / `test_case_insensitive_parse` | 改任一 `as_wire` 拼写；删 `rename_all = "lowercase"`；把 `default` 当合法档位 |
| `ys-protocol`（`request.rs`） | `test_request_set_effort_roundtrip`（`Some` / `None` 两形态）；`test_legacy_request_variants_still_deserialize` 含 `SetEffort` 裸 JSON | 漏测 / 改外部标签；`None` 形态无法反序列化 |
| `ys-session`（`meta.rs`） | `effort_serde_roundtrip`（落盘小写）；`effort_is_optional_and_backward_compatible`（缺键 / `null`） | 删 `#[serde(default)]`；把字段改成非 `Option` |
| 适配器（`lib.rs` / `compat.rs`） | `reasoning_effort_is_optional_on_the_wire`（`None` 不出现 / 显式值原样）；`constructors_pin_reasoning_effort_support` | 去掉 `skip_serializing_if`；把 `opencode_go` 的能力位关掉或给其它 provider 打开 |
| app 能力（`capabilities.rs`） | `set_effort_persists_and_rebuilds_model` / `set_effort_none_shows_default` / `set_effort_without_credentials_clears_wiring_model` / `set_effort_warns_when_provider_ignores_it` / `resume_session_restores_effort_from_meta` | 只写 config 不重建；缺凭证仍保留旧 model；resume 漏写 `cfg.effort` |
| app 配置（`config.rs`） | `reasoning_effort_is_optional_and_validated`；`effective_reasoning_effort` / `effective_effort_level` 的覆盖与回落断言 | 覆盖优先级反转；default 不回落 `high` |
| app 循环（`app_loop.rs`） | `set_effort_updates_view`（覆盖更新、清除后回到默认）；`every_request_variant_is_handled` 覆盖该变体；`turn_end_persists_effort_to_meta`；`effort_survives_new_session` | 漏 `SetEffort` 臂 ⇒ 编译失败；回合末不写 meta；`/new` 重置覆盖 |
| 会话收尾（`main.rs`） | `finalize_turn_meta_effort_three_states`；`restore_effort_from_meta_three_states` | 无条件写 ⇒ 三态用例变红；启动恢复漏 meta |
| 子 Agent（`subagent.rs`） | `named_model_cache_key_includes_effort`（默认 high 与覆盖 max 各构建一次） | 键退回只用模型名 ⇒ 第二次命中旧实例 |
| 视图（`app` / TUI） | `test_build_view_passes_effort`；`test_effort_parse_forms`；`test_apply_local_effort_shows_current_level_and_usage` / `..._shows_default_when_unset`；`test_status_text_reports_effort`；`test_command_table_shape`（14 条）；`test_effort_with_argument_sends_set_effort_only`；`test_bare_effort_writes_one_local_line_and_sends_nothing` | 视图写死 `None`；删 `default` 分支；把 `/effort` 接进 `/thinking` 的镜像路径；无参也发请求 |
| 文档一致性 | `scripts/doc-consistency.sh` 断言命令表含 `/effort`、计数 14 条 | 计数回退或命令表漏行 ⇒ 脚本非零退出 |

## 7. 已知边界

- **能力门控是 provider 级而非模型级**：`supports_reasoning_effort` 只看 provider；
  同 provider 下个别模型不支持时仍会发送。真实接受度以 provider 手测为准。
- **`off` 未必有真正的 disable 语义**：`off` 只把 `"off"` 作为拼写发出去；部分模型 /
  网关会忽略或强制思考，本仓不做兜底。
- **只影响后续回合**：设置不打断进行中的回合，也不在回合内热切换模型。
- **设置单独不落盘**：TUI 中只执行 `/effort` 而不跑回合，会话 meta 不变。
- **不做**：`-p` / `--json` 的 `--effort` flag、`YUSHAN_EFFORT` 环境变量（配置默认
  仍用 `YUSHAN_REASONING_EFFORT`）、按问题难度自动路由、能力发现、请求被拒后的
  自动降档重试。
- 服务端对档位的折叠 / 忽略（例如把 `minimal` 提升到更高档）不在本仓控制范围内。
