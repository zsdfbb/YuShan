# `/effort` 思考档位 —— as-built

`/effort` 在会话内设置模型的思考深度，随会话 meta 持久化。它的规范取值由
`ys-core::EffortLevel` 唯一给出；各 provider 的 wire 方言由模型适配器翻译，核心类型
不感知 provider。

## 1. 行为契约

| 输入 | TUI 行为 | 发给 app | 输出 |
|---|---|---|---|
| `/effort` | 本地读 `CodingView.effort`，写**恰好一条** System 行（当前档位 + 用法），**不发请求** | — | `effort: <level>` 或 `effort: default`，附用法串 |
| `/effort <level>` | 解析为七个规范档位之一后直发 | `SetEffort { effort: Some(level) }` | `effort: <level>` |
| `/effort default` | 大小写不敏感地识别清除令牌 | `SetEffort { effort: None }` | `effort: default` |
| `/effort <非法值>` | 本地用法提示，**不改变任何状态**、不发请求 | — | `用法：/effort [off|minimal|low|medium|high|xhigh|max|default]` |

- 合法档位：`off` / `minimal` / `low` / `medium` / `high` / `xhigh` / `max`。
  解析前 `trim`、大小写不敏感；**`default` 不是档位**，而是 `Option<EffortLevel>` 的
  `None`（清除设置、恢复 provider 默认）。
- `xhigh` 是规范拼写（无连字符）。`EffortLevel::as_wire` 与 serde 的
  `rename_all = "lowercase"` 是同一契约的两种呈现。
- 无参显示**只读视图**：TUI 不持有第二份档位真值，不先于 app 改写本地状态。
- `/status` 固定含一行 `effort: default` 或 `effort: <level>`（规范拼写，非 `Debug` 名）。
- 设置后**下一回合**生效：回合使用回合开始时取定的模型实例，回合中不热切换。

## 2. 数据流

```
TUI 输入
  └─ commands::parse → Action::Request(Request::SetEffort { effort })
       └─ app_loop（信道 ①）→ match Request::SetEffort
            └─ capabilities::set_effort(&mut Config, &mut Wiring, effort)
                 ├─ config.effort = effort
                 ├─ config.build_model() → wiring.set_model(Some | None)
                 └─ Vec<String> → Outbound::Output；随后 Outbound::View（CodingView.effort）
                      └─ 下一回合：Config model factory 把 cfg.effort 交给
                         OpenAICompatibleConfig.effort
                           └─ adapter::translate_request → reasoning_params(方言, effort)
                                └─ ChatCompletionRequest { reasoning_effort, thinking }
                      └─ 回合末：finalize_turn_meta(..., Some(config.effort), ...)
                           └─ SessionMeta.effort（TUI 模式）
```

- **运行期真值**是 `Config.effort`；`Wiring.model` 是当前回合实际使用的模型实例。
  档位落在 `OpenAICompatibleConfig` 上，故只有重建 model 才会进入请求。
- 模型工厂（主模型与 `build_model_for`）都读 `cfg.effort`，因此 subagent、压缩 /
  摘要等经工厂构造的调用**自动继承**当前档位。
- 重建失败（缺凭证）⇒ `wiring.set_model(None)`，与 `/model` 同一契约：绝不留下
  与 `Config` 不一致的旧模型实例。
- `capabilities.rs` 不 print：所有用户可见行经 `Vec<String>` → `Outbound::Output`
  → transcript。
- 启动期 `--resume` 与运行期 `/resume` 都从目标会话 meta 读 `effort`：
  `build_candidate_model` 用「候选四字段 + 候选档位」构建成功后，提交步才写回
  `cfg.effort`；失败路径不改动当前配置。

### 子 Agent 与辅助调用

- 主模型的 `Config::build_model` 与按名构造子 Agent 的 `Config::build_model_for`
  都读 `cfg.effort`；未指定模型名的子 Agent 直接复用父模型实例，因此同样继承
  回合开始时的档位。
- 具名子 Agent 的模型实例缓存键是 `(模型名, 构建时 effort)`：`/effort` 变化后，
  同名派发会以新档位重建实例，不会命中旧档位的实例。缓存跨派发复用连接池与 TLS。
- 已知边界：缓存键不含 provider / api_base，二者变化不会使同名子 Agent 的缓存
  实例失效（既有局限，不在 `/effort` 的范围内）。

## 3. provider wire 方言

`ProviderCompat.reasoning_dialect` 决定字段组合；`reasoning_params` 是唯一翻译点。

| 方言 | 对应 provider | `off` | 普通档位 | `default`（`None`） |
|---|---|---|---|---|
| `DeepSeek` | `deepseek` | 只发 `thinking: {"type":"disabled"}` | `reasoning_effort: <level>` **且** `thinking: {"type":"enabled"}` | 两字段皆不发 |
| `OpenCodeGo` | `opencode-go` | 只发 `thinking: {"type":"disabled"}` | **只**发 `reasoning_effort: <level>` | 两字段皆不发 |
| `OpenAiEffort` | `custom` / env 直连 / 未知端点 | `reasoning_effort: "none"` | `reasoning_effort: <level>` | 两字段皆不发 |
| `None` | `minimax` | 不发任何思考字段 | 不发任何思考字段 | 两字段皆不发 |

- 档位 wire 拼写：`minimal|low|medium|high|xhigh|max`；服务端折叠（例如把 `medium`
  当作 `high`）由 provider 负责，适配器不做本地映射。
- **OpenCodeGo 互斥**：该网关同时收到 `reasoning_effort` 与 `thinking` 会 400，故
  `off` 只发 toggle、普通档位只发 effort，**任何档位都不同时发送两字段**。
- `ReasoningDialect::None` 在 `effort = Some(_)` 时也静默不发；命令层另给一条
  warning（见 §5）。
- `effort = None` 的判定先于方言分派：任何方言都不发送思考参数。

## 4. 持久化语义

- 落点：`ys-session::SessionMeta.effort: Option<EffortLevel>`，JSONL 首行。
  `#[serde(default)]` 是回写兼容契约：缺该键的旧首行反序列化为 `None`，显式
  `null` 同样为 `None`。
- **TUI 模式**：回合末由 `finalize_turn_meta` 写穿。参数是三态：
  - `Some(Some(level))` ⇒ 写入档位；
  - `Some(None)` ⇒ 清除已持久化的档位；
  - `None` ⇒ 不写（`-p` / `--json`）。
  因此单独执行 `/effort` 而不跑回合不会落盘；下一次成功回合才写入。
- **`/new` 不重置**：只换会话句柄，`config.effort` 保持；新会话首个回合末把当前
  档位写入新 meta（与 `thinking_visible` 同规）。
- **`/resume` 还原**：无参列表选择与有参直切都读目标 meta 的 `effort`，档位 `None`
  即清除当前设置，并在提交前用候选档位构建模型。
- **启动期 `--resume`**：`config.effort = resume_meta.effort`；无 resume meta 时保持
  `None`（provider 默认）。
- **`-p` / `--json` 不写**：两者没有 `/effort` 命令，收尾时向 `finalize_turn_meta`
  传外层 `None`，不覆盖既有会话 meta。若启动时 `--resume` 读到档位，则本次请求会
  按该档位发送。
- **不进 `config.json`**：`Config.effort` 不是用户配置项，只由运行期命令 / 会话
  meta 赋值。

## 5. 关键不变量

- `ys-core::EffortLevel` 无 tokio、无 provider 知识；它是请求侧档位拼写的唯一来源。
- `EffortLevel::from_str` **严格**接受七个规范拼写，拒绝 `default` 与未知值；错误
  保留原始输入并列出全部合法值。
- `config.effort` 是运行期真值；设置后必须重建 `Wiring.model`，否则请求不会改变。
- OpenCodeGo 方言下 `reasoning_effort` 与 `thinking` **绝不同时出现**。
- `set_effort` 在缺凭证时清空 wiring model，绝不保留与 config 不一致的旧实例。
- 不支持的 provider 只给 warning，设置**照常保存**：切到支持的 provider 后立即生效。
  warning 判据是 `Config::supports_reasoning_effort()`（方言是否为 `None`），不改请求。
- `capabilities.rs` 不 print；命令期输出只走 `Outbound::Output`。
- 旧 JSONL（无 `effort` 键）必须仍可反序列化。

## 6. 测试覆盖与变异点

| 层 | 用例 | 变异（注入 ⇒ 预期变红） |
|---|---|---|
| `ys-core`（`effort.rs`） | `test_all_levels_parse_and_display_roundtrip` / `test_serde_json_roundtrip` / `test_invalid_values_rejected` / `test_case_insensitive_parse` | 改任一 `as_wire` 拼写；删 `rename_all = "lowercase"`；把 `default` 当合法档位 |
| `ys-protocol`（`request.rs`） | `test_request_set_effort_roundtrip`（`Some` / `None` 两形态）；`test_legacy_request_variants_still_deserialize` 含 `SetEffort` 裸 JSON | 漏测 / 改外部标签；`None` 形态无法反序列化 |
| `ys-session`（`meta.rs`） | `effort_serde_roundtrip`（落盘小写）；`effort_is_optional_and_backward_compatible`（缺键 / `null`） | 删 `#[serde(default)]`；把字段改成非 `Option` |
| 适配器（`lib.rs` / `compat.rs`） | `reasoning_params_match_every_dialect_and_level`（4 方言 × 7 档）；`no_effort_means_no_reasoning_params_for_every_dialect`；`constructors_pin_reasoning_dialect`；`opencode_go_parses_deepseek_reasoning_content_and_stream_usage` | OpenAI `off` 改回 `"off"`；映射写成固定值；DeepSeek `off` 补发档位；OpenCodeGo 档位分支补 `thinking`；`None` 方言走 OpenAI 映射 |
| app 能力（`capabilities.rs`） | `set_effort_persists_and_rebuilds_model` / `set_effort_none_shows_default` / `set_effort_without_credentials_clears_wiring_model` / `set_effort_warns_when_provider_ignores_it` / `resume_session_restores_effort_from_meta` | 只写 config 不重建；缺凭证仍保留旧 model；resume 漏写 `cfg.effort` |
| app 循环（`app_loop.rs`） | `set_effort_updates_view`（`Some` 更新、`None` 清除）；`every_request_variant_is_handled` 覆盖该变体 | 漏 `SetEffort` 臂 ⇒ 编译失败；只改 config 不刷视图 |
| 会话收尾（`main.rs`） | `finalize_turn_meta_effort_three_states` | 无条件写 ⇒ 三态用例变红 |
| 视图（`app` / TUI） | `test_build_view_passes_effort`；`test_effort_parse_forms`；`test_apply_local_effort_shows_current_level_and_usage` / `..._shows_default_when_unset`；`test_status_text_reports_effort`；`test_command_table_shape`（14 条）；`test_effort_with_argument_sends_set_effort_only`；`test_bare_effort_writes_one_local_line_and_sends_nothing` | 视图写死 `None`；删 `default` 分支；把 `/effort` 接进 `/thinking` 的镜像路径；无参也发请求 |
| 文档一致性 | `scripts/doc-consistency.sh` 断言命令表含 `/effort`、计数 14 条 | 计数回退或命令表漏行 ⇒ 脚本非零退出 |

## 7. 已知边界

- **方言未实测**：`opencode-go` 对顶层 `reasoning_effort` 的接受度、DeepSeek 工具
  循环是否要求回传 `reasoning_content`，需真实 provider 手测确认。映射集中在
  `ProviderCompat` / `reasoning_params` 一处，调整不影响其它层。适配器当前不把
  `reasoning_content` 回放进后续请求。
- **`off` 未必有真正的 disable 语义**：部分模型 / 网关会忽略关闭请求或强制思考；
  以实际 provider 行为为准，本仓不做兜底。
- **warning 是方言级而非模型级**：`supports_reasoning_effort()` 只看
  `ReasoningDialect`，不探测单个模型的能力；同 provider 下个别模型不支持时仍会发送。
- **只影响后续回合**：设置不打断进行中的回合，也不在回合内热切换模型。
- **设置单独不落盘**：TUI 中只执行 `/effort` 而不跑回合，会话 meta 不变。
- **不做**：`-p` / `--json` 的 `--effort` flag、`YUSHAN_EFFORT` 环境变量、按问题难度
  自动路由、能力发现、请求被拒后的自动降档重试。
- 服务端对档位的折叠 / 忽略（例如把 `minimal` 提升到更高档）不在本仓控制范围内。
