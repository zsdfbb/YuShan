# `/effort` 思考档位 —— as-built

`/effort` 在会话内覆盖模型的思考强度。规范取值由 `ys-core::EffortLevel` 唯一给出；
请求实际发送的字段仍是 provider 的 `reasoning_effort`，默认值沿用 ADR-0042（`high`）。
发送与否经**两层**门控：provider 级 `supports_reasoning_effort` + 模型级
`supports_effort(model)`，实际下发值可能再经方言**折叠**（`effort_for`）。

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
                         → reasoning_effort_for(compat, model, effective)
                              └─ ProviderCompat::effort_for(model, configured)
                                   ├─ supports_reasoning_effort = false ⇒ 不发送（命令层给 warning）
                                   ├─ supports_effort(model) = false ⇒ 不发送（同上）
                                   ├─ EffortDialect::Verbatim ⇒ 原样透传
                                   └─ EffortDialect::Glm ⇒ 按代际折叠（见 §3）
                      └─ 回合末：finalize_turn_meta(..., Some(config.effort), ...)
                           └─ SessionMeta.effort（只存**覆盖值**；TUI 模式）
```

- **运行期真值**：有效强度 = `Config.effort`（会话覆盖）或
  `Config.reasoning_effort`（配置默认）。`Wiring.model` 是当前回合实际使用的模型实例。
- **有效强度 ≠ wire 值**：有效强度是用户意图（七个规范档位之一）；wire 值是门控与
  折叠之后真正序列化的字符串，可能不等于前者（`glm-5.3` 上 `xhigh` → `max`）或根本
  不存在（id 无法解析出 GLM 代际时整个字段不出现）。`OpenAICompatibleConfig.reasoning_effort` 收到的
  是后者 —— 故只有重建 model 才会进入请求。日志与调试事件（`run_start` 的
  `gen_ai.request.reasoning_effort`、`app_loop` 的 `model_changed.reasoning_effort`）也
  一律记 wire 值：记原始配置值会让排障得出与真实请求相反的结论。
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

## 3. 能力门控：provider 级 + 模型级（ADR-0042）

门控是**两层**，两层都过才发字段：

1. **provider 级**：`ProviderCompat.supports_reasoning_effort` —— 该端点认不认这个字段。
   当前 `opencode-go` 与 `glm` / `glm-coding` / `mimo` 为 `true`；`deepseek` / `minimax` / custom /
   未知端点默认 `false`，避免严格端点因未知顶层字段返回 400。
2. **模型级**：`ProviderCompat.supports_effort(model)` —— 该**模型**接受与否。
   `EffortDialect::Verbatim`（默认）没有模型级信息，信任能力位；`EffortDialect::Glm`
   只要求 id 能解析出代际，无法从 id 解析出代际时 **fail-closed**（不发）。

实际下发值由 `ProviderCompat::effort_for(model, configured)` 唯一给出，两个 model factory
（主模型与 `build_model_for`）都经 `main::reasoning_effort_for(compat, model, configured)`
调它。`Config::accepts_reasoning_effort()` 是面向用户的同一判定（`supports_effort`），
`/effort` 的 warning 与它同源。

**GLM 折叠表**（`EffortDialect::Glm`）：厂家规定 `glm-5.3` 只接受 `low` / `high` / `max`，
非法值直接 400（不静默折叠，故折叠必须由本地做）：

| 配置值 | `glm-5.3` / `glm-5.3-flash` | `glm-5.2` | `< (5,2)`（手敲更早的 id 时可达） |
|---|---|---|---|
| `off` | `low` | `minimal` | `minimal` |
| `minimal` | `low` | `minimal` | `minimal` |
| `low` | `low` | `high` | `low`（原样） |
| `medium` | `high` | `high` | `medium`（原样） |
| `high` | `high` | `high` | `high`（原样） |
| `xhigh` | `max` | `max` | `xhigh`（原样） |
| `max` | `max` | `max` | `max`（原样） |

- 白名单之外的取值（上游配置错误）**不猜映射**，直接不发。
- `< (5,2)` 分支：更早的代际实测接受全集（含 `xhigh` / `max`），故原样透传；`off` 仍不是
  合法 wire 值，必须映射为 `minimal`。真实目录里**仍有更早代际的条目**（只是不进前 3 项展示），
  故该分支在用户手敲这些 id 时可达，不是死代码。
- 无法从 id 解析出代际 ⇒ 整个字段不序列化。
- `Verbatim` 方言**逐字透传**（不 trim、不改写）：GLM 的折叠不得外溢到 `opencode-go`
  一类端点。

**MiMo 折叠表**（`EffortDialect::Mimo`）：实测该端点的合法集为 `none|low|medium|high|max`，
`minimal` / `xhigh` / `off` 直接 400（不静默折叠）：

| 配置值 | MiMo wire |
|---|---|
| `off` | `low` |
| `minimal` | `low` |
| `low` | `low`（原样） |
| `medium` | `medium`（原样） |
| `high` | `high`（原样） |
| `xhigh` | `max` |
| `max` | `max`（原样） |

- `off` 映射到 `low`（最低档）而**不是** `none`：`none` 是「关闭思考」，与 `off` 的
  「最低思考强度」语义不同 —— 与 GLM 把 `off` 映射到 `minimal` 同一原则。
- ⚠️ 该端点的 400 错误文案写「must be one of 'none', 'low', 'medium' or 'high'」，
  **漏了 `max`** —— 实测 `max` 返回 200。故以实测为准，不按文案把 `max` 也排除。
- 模型级门控：id 必须能识别为 MiMo 的 chat 模型（`mimo-` 前缀**且**前缀后有实际模型名
  **且**不是语音模型**且**不是禁用模型），否则 fail-closed。
- 详见 `docs/design-final/mimo.md`。

- 适配器只做一件事：`OpenAICompatibleConfig.reasoning_effort` 原样序列化为顶层
  `reasoning_effort`；`None` 时整个字段不序列化。门控与折叠**全在 compat 层**，适配器
  不知道档位语义。
- 档位 wire 拼写：`off|minimal|low|medium|high|xhigh|max`。`Verbatim` 方言下服务端折叠
  （例如把 `medium` 当作 `high`）由 provider 自行负责；`Glm` / `Mimo` 方言的折叠由本仓做。
- 不接受该字段时（provider 级或模型级）：设置**照常保存**；命令层输出一条 warning，
  同时报出 provider **与 model**（同一 provider 上不同代际接受与否不同，只说「provider
  不支持」会把用户引到错的修正方向）。切到接受该字段的 provider / 模型后立即生效。

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
  字段；`supports_effort(model) = false` 的模型同样。命令层只给 warning，不改请求。
- **门控只在一处判**：`ProviderCompat::effort_for` 内部调 `supports_effort`，上层（model
  factory / 日志 / 命令层）都不得自行另写一份判定 —— 否则「日志说发了」与「请求真发了」
  会漂移。
- **折叠只能发生在方言层**：`Verbatim` 必须逐字透传。给 `Glm` 加档位不得改变其它
  provider 的 wire 值。
- `set_effort` 在缺凭证时清空 wiring model，绝不保留与 config 不一致的旧实例。
- `capabilities.rs` 不 print；命令期输出只走 `Outbound::Output`。
- 旧 JSONL（无 `effort` 键）必须仍可反序列化。

## 6. 测试覆盖与变异点

| 层 | 用例 | 变异（注入 ⇒ 预期变红） |
|---|---|---|
| `ys-core`（`effort.rs`） | `test_all_levels_parse_and_display_roundtrip` / `test_serde_json_roundtrip` / `test_invalid_values_rejected` / `test_case_insensitive_parse` | 改任一 `as_wire` 拼写；删 `rename_all = "lowercase"`；把 `default` 当合法档位 |
| `ys-protocol`（`request.rs`） | `test_request_set_effort_roundtrip`（`Some` / `None` 两形态）；`test_legacy_request_variants_still_deserialize` 含 `SetEffort` 裸 JSON | 漏测 / 改外部标签；`None` 形态无法反序列化 |
| `ys-session`（`meta.rs`） | `effort_serde_roundtrip`（落盘小写）；`effort_is_optional_and_backward_compatible`（缺键 / `null`） | 删 `#[serde(default)]`；把字段改成非 `Option` |
| 适配器（`lib.rs` / `compat.rs`） | `reasoning_effort_is_optional_on_the_wire`（`None` 不出现 / 显式值原样）；`constructors_pin_reasoning_effort_support`；`effort_dialect_defaults_to_verbatim`；`verbatim_passes_configured_value_through` | 去掉 `skip_serializing_if`；把 `opencode_go` 的能力位关掉或给其它 provider 打开；把 `Glm` 设为默认方言 |
| 适配器 GLM 折叠（`compat.rs`） | `glm_capabilities`；`glm_5_3_folds_every_tier` / `glm_5_2_folds_every_tier`（逐档）；`glm_ids_below_5_2_pass_through_but_never_send_off`（`< (5,2)` 分支）；`glm_gates_effort_by_model_identity`（无法解析 ⇒ fail-closed）；`glm_never_emits_off_on_the_wire`；`glm_generation_parse_is_case_insensitive` | 改任一档的映射；把 `off` 在 5.3 上映射成 `minimal`；去掉模型级门控或把它改成 fail-open；把代际解析改成大小写敏感 |
| app 能力（`capabilities.rs`） | `set_effort_persists_and_rebuilds_model` / `set_effort_none_shows_default` / `set_effort_without_credentials_clears_wiring_model` / `set_effort_warns_when_provider_ignores_it` / `set_effort_warns_when_model_rejects_effort` / `resume_session_restores_effort_from_meta` | 只写 config 不重建；缺凭证仍保留旧 model；resume 漏写 `cfg.effort`；把 `accepts_reasoning_effort()` 换回 `supports_reasoning_effort()`（无法解析的模型那条 warning 消失）；删 warning 里的 `{model}` |
| app 配置（`config.rs`） | `reasoning_effort_is_optional_and_validated`；`effective_reasoning_effort` / `effective_effort_level` 的覆盖与回落断言；`test_accepts_reasoning_effort_is_model_level` / `test_accepts_reasoning_effort_differs_from_provider_level` | 覆盖优先级反转；default 不回落 `high`；把 `accepts_reasoning_effort` 退回只看能力位 |
| app 调试日志 / 启动事件（`app_loop.rs` / `main.rs`） | `model_changed_records_wire_effort`；`run_start_records_wire_reasoning_effort`；`reasoning_effort_is_gated_by_provider_compat` | 记原始配置值而非 wire 值（`xhigh` 不会变 `max`、无法解析的 id 不会变 `null`）；把 `Glm` 方言误用到所有 provider（`opencode-go` 的 `xhigh` 锚点变成 `max`） |
| app 循环（`app_loop.rs`） | `set_effort_updates_view`（覆盖更新、清除后回到默认）；`every_request_variant_is_handled` 覆盖该变体；`turn_end_persists_effort_to_meta`；`effort_survives_new_session` | 漏 `SetEffort` 臂 ⇒ 编译失败；回合末不写 meta；`/new` 重置覆盖 |
| 会话收尾（`main.rs`） | `finalize_turn_meta_effort_three_states`；`restore_effort_from_meta_three_states` | 无条件写 ⇒ 三态用例变红；启动恢复漏 meta |
| 子 Agent（`subagent.rs`） | `named_model_cache_key_includes_effort`（默认 high 与覆盖 max 各构建一次） | 键退回只用模型名 ⇒ 第二次命中旧实例 |
| 视图（`app` / TUI） | `test_build_view_passes_effort`；`test_effort_parse_forms`；`test_apply_local_effort_shows_current_level_and_usage` / `..._shows_default_when_unset`；`test_status_text_reports_effort`；`test_command_table_shape`（14 条）；`test_effort_with_argument_sends_set_effort_only`；`test_bare_effort_writes_one_local_line_and_sends_nothing` | 视图写死 `None`；删 `default` 分支；把 `/effort` 接进 `/thinking` 的镜像路径；无参也发请求 |
| 文档一致性 | `scripts/doc-consistency.sh` 断言命令表含 `/effort`、计数 14 条 | 计数回退或命令表漏行 ⇒ 脚本非零退出 |

## 7. 已知边界

- **`off` 未必有真正的 disable 语义**：`Verbatim` 方言下 `off` 只把 `"off"` 作为拼写
  发出去，部分模型 / 网关会忽略或强制思考；`Glm` 方言下 `off` 在 5.3 上折叠为 `low`
  （该代际不支持关闭思考）。本仓不做其它兜底。
- **模型级门控只认能从 id 解析出的代际**：`Glm` 方言的判据是 `glm-<major>.<minor>` 前缀。
  用户在 `config.json` / `/model` 里手敲一个无法解析的别名（如给 `glm` provider 配一个
  中转层的自定义 id）会 fail-closed —— **不发** `reasoning_effort`。这是有意的：宁可少发，
  也不吃 400。
- **折叠表是静态的**：厂家新增档位或改接受集合时必须在 `ProviderCompat::effort_for` 与
  用例里同步更新；没有能力发现机制。
- **只影响后续回合**：设置不打断进行中的回合，也不在回合内热切换模型。
- **设置单独不落盘**：TUI 中只执行 `/effort` 而不跑回合，会话 meta 不变。
- **不做**：`-p` / `--json` 的 `--effort` flag、`YUSHAN_EFFORT` 环境变量（配置默认
  仍用 `YUSHAN_REASONING_EFFORT`）、按问题难度自动路由、能力发现、请求被拒后的
  自动降档重试。
- 未做本地映射的 provider（`Verbatim` 方言）对档位的折叠 / 忽略不在本仓控制范围内。
