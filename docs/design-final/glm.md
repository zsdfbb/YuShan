# 智谱 GLM 接入（`glm` / `glm-coding`）— As-built

> 性质：**实际建成记录**。provider 注册表与 `/login` 的既有契约见
> `docs/design-final/opencode-go.md` 与 `docs/arch/commands/login-model-behavior/design.md`；
> `reasoning_effort` 的规范、门控与持久化见 `docs/design-final/effort.md`。日期：2026-10-26

## 一句话

新增两个内置 provider —— `glm`（标准 API）与 `glm-coding`（Coding Plan 专属端点）—— 并为
适配器补上**按模型代际门控并折叠** `reasoning_effort` 的能力。与既有 provider 的两个关键差别：
① `reasoning_effort` 的可用取值随代际收缩，且 `off` 不是该端点的合法 wire 值；
② 两个端点的目录能力不同 —— 通用端点有 `/v1/models`，编码套餐端点没有（实测 404），
后者只能靠 `CatalogSource::Seed` 兜底。

## 厂商契约（外部依据）

| 维度 | 行为 |
|---|---|
| 端点（标准 API） | `https://open.bigmodel.cn/api/paas/v4`（请求打 `<base>/chat/completions`） |
| 端点（Coding Plan） | `https://open.bigmodel.cn/api/coding/paas/v4` |
| 认证 | `Authorization: Bearer <key>`；**无**额外会话头。两个端点的 key **不通用**（个人套餐 / 团队套餐 / 标准 API 分开管理） |
| 模型目录（通用端点） | `GET /api/paas/v4/v1/models` → **200，17 条**。字段含 `id` / `name` / **`context_length`** / `max_output_length` / `pricing` / `description` / `created` 等；**没有** `context_window`（本仓内部名）。`GET /api/paas/v4/models` 也有，但只报 `id` / `owned_by` / `created`，**无窗口** |
| 模型目录（编码套餐端点） | `GET /api/coding/paas/v4/v1/models` → **404**；`.../coding/paas/v4/models` → 200 但无窗口。故该端点只能靠种子 |
| 目录顺序 | 服务端返回顺序**稳定**（连续请求一致），但**既非** `created` 排序、**也不**字母序。实测前几项为 `glm-5.3-flash` / `glm-5.3-flashx` / `glm-5.3` / `glm-5.2` / … |
| 流式 | SSE，以 `data: [DONE]` 收尾；**末包**带 `usage`（含 `prompt_tokens_details.cached_tokens`）。实测接受 `stream_options.include_usage`，`supports_stream_usage` 保持开启 |
| 思考字段 | `delta.reasoning_content` 是思考增量（与 DeepSeek 同形，非 `reasoning`）。实测确认 |
| `reasoning_effort` | `glm-5.3` 家族（含 `-flash` / `-flashx`）只接受 `low` / `high` / `max`，其余值返回 `code 1210`（不静默折叠）；`glm-5.2` 及更早代际接受全集。**`off` 不是合法 wire 值**（合法集为 `none\|minimal\|low\|medium\|high\|xhigh\|max`，传 `off` 报 `code 1210`），故任何代际都必须先本地映射 |
| 上下文 / 输出 | `glm-5.3` / `glm-5.3-flash` / `glm-5.3-flashx` / `glm-5.2` 均为 1,048,576 上下文 / 131,072 输出 |

## 落点

| 文件 | 改动点 |
|---|---|
| `adapters/model-openai-compatible/src/compat.rs` | `EffortDialect { Verbatim, Glm }`（`Default = Verbatim`）；`ProviderCompat.effort_dialect` 字段；`ProviderCompat::glm()`；模型级门控 `supports_effort(model)`；实际下发值 `effort_for(model, configured)`；`glm_generation` / `read_digits` 解析代际 |
| `apps/coding-agent/src/provider.rs` | `CatalogSource { Endpoint, Seed(&'static [(&'static str, usize)]) }` + `ProviderInfo.catalog`；`GLM_SEED_MODELS`；`glm` / `glm-coding` 两个内置项；`ProviderRegistry::new()` 构造期把种子写进 `model_cache`；`seed_catalog(name)`；`fetch_targets` 跳过 `Seed` |
| `apps/coding-agent/src/app_loop.rs` | `Request::Login` 按 `seed_catalog` 分两条路径；`record_model_change` 的 `model_changed.reasoning_effort` 记 **wire 值** |
| `apps/coding-agent/src/config.rs` | `accepts_reasoning_effort()`（= `current_compat().supports_effort(&self.model)`，模型级） |
| `apps/coding-agent/src/capabilities.rs` | `/effort` 的 warning 改用模型级门控，文案含 provider **与 model** |
| `apps/coding-agent/src/main.rs` | `reasoning_effort_for(compat, model, configured)` 委派 `compat.effort_for`；两个 model factory 都传模型名；启动期 `gen_ai.request.reasoning_effort` 记 wire 值 |
| `apps/coding-agent/src/view.rs` | 内置 provider 计数 6；`glm` / `glm-coding` 均有内置 base（`/login` 不再追问 URL） |

## 关键行为（实现期定死，勿凭直觉改）

**为什么需要 `CatalogSource::Seed`**：本仓库的既有契约是「模型名**只有目录一个来源**」——
`build_view` 绝不拿写死的名字充数（写死的名字必然过时，选中即 400）。该厂商拿不到目录，
若在视图层另开一条「内置名单」分支，就会变成两个来源。故种子被写进 `model_cache`
（键为 `cache_key(api_base)`，尾斜杠不计），于是 `models_for_provider` / `models_for_base` /
`catalog_context_window` / `provider_for_model` **一行都不必为种子改**：种子**就是**目录。

`Seed` 与 `Endpoint` 并存而非互斥：种子先进缓存，将来端点真能列出目录时
`apply_model_cache` 会用真实目录**整体覆盖**同键条目（不是合并 —— 种子里没被新目录列出的
条目一并消失）。种子条目没有 `owned_by` / 展示名，只有 `context_window`。

**种子的作用只是兜底**（`glm-5.3` / `glm-5.3-flash`）：通用端点有 `/v1/models`，真实目录会覆盖种子；
只有拿不到目录的端点（编码套餐，实测 404）才用到种子。种子**顺序即兜底情形下的起始模型** ——
`/login` 取目录第一条，故无目录时登录后模型是 `glm-5.3`。

**为什么 `fetch_targets` **不**跳过种子 provider**：种子只是兜底，真实目录才是模型名的第一手
来源（含 `context_length` 与服务端顺序）。排除它们会让种子**永久遮蔽**真实目录 ——
`apply_model_cache` 根本没机会覆盖。代价仅是没有目录接口的端点多一次静默超时，
而拉取本身已被 `JoinSet` 并行。

**为什么 `reasoning_effort` 必须按模型代际门控 + 折叠**：能力位（`supports_reasoning_effort`）
只答「这个 provider 认不认该字段」，而该厂商的**可用取值随代际收缩**，非法值直接 400
（不静默折叠）。故两层：

| 层 | 判据 | 落点 |
|---|---|---|
| provider 级 | `ProviderCompat.supports_reasoning_effort`（`glm()` 为 `true`） | 决定字段是否有资格进请求 |
| 模型级 | `supports_effort(model)`：`Verbatim` 信任能力位；`Glm` 只要求 id 能解析出代际，代际无法从 id 解析时 **fail-closed** | 决定这一个模型发不发 |

折叠表（`effort_for` 对 `EffortDialect::Glm` 的唯一实现）：

| 配置值 | `glm-5.3` / `glm-5.3-flash` | `glm-5.2` | `< (5,2)`（手敲更早的 id 时可达） |
|---|---|---|---|
| `off` | `low` | `minimal` | `minimal` |
| `minimal` | `low` | `minimal` | `minimal` |
| `low` | `low` | `high` | `low`（原样） |
| `medium` | `high` | `high` | `medium`（原样） |
| `high` | `high` | `high` | `high`（原样） |
| `xhigh` | `max` | `max` | `xhigh`（原样） |
| `max` | `max` | `max` | `max`（原样） |

- 5.3 家族的 `off` 折叠为 `low`：该代际不再支持关闭思考，`low` 是它能接受的最低档。
- 白名单之外的取值（上游配置错误）**不猜映射**，直接不发 —— 与其发一个必然 400 的值，
  不如退化为「不带该字段」。
- 代际解析（`glm_generation`）大小写不敏感，`glm-` 之后取 `<major>.<minor>`，minor 后可跟
  任意字符（`glm-5.3-flash` → `(5, 3)`，`glm-5.2v` → `(5, 2)`），省略 minor 的 id
  （`glm-5`）按 `(5, 0)` 处理；缺前缀或缺 major ⇒ `None` ⇒ fail-closed。
- 第三列（`< (5,2)` 原样透传 + `off` 映射）是真实存在的代码分支：真实目录仍包含更早代际，
  它们不在前 3 项展示内，但用户手敲时仍可选中 —— 届时合法档位原样发、`off` 映射为 `minimal`。
- `Verbatim` 方言**逐字透传**（不 trim、不改写）：`glm` 的折叠规则不得外溢到 `opencode-go`
  一类端点。

**为什么 `glm` 与 `glm-coding` 是两个 provider**：两个端点的 key 不通用，必须分开登录、
各持一份 auth 条目；共用一个 provider 会让编码套餐的 key 被发到通用端点。两者的种子目录
相同（同一套模型 id 与窗口），故 `/model` 里同名模型**各占一行**，靠 label 的 provider
前缀区分。

**`model_changed` 日志记 wire 值而非原始配置值**：`reasoning_effort` 字段经门控 + 折叠
之后才落 `serde_json` —— `glm-5.3` 的 `xhigh` 显示为 `max`，id 解析不出代际的模型显示为
`null`。记原始值会让排障得出与真实请求**相反**的结论。同一原因．`supports_reasoning_effort`
字段已从该事件移除：能力位（provider 级还是模型级、认不认该字段）已被 wire 值的
`null` / 非 `null` 完全表达，再记一个会与它互相矛盾。

## 两条配置路径

```bash
# ① 持久（推荐）：TUI 里 /login glm → 填 key；起始模型取种子第一条（glm-5.3），
#    目录进 registry 缓存，/model 候选与 1M 窗口立刻可用。**全程零网络请求**。
#    编码套餐同理走 /login glm-coding（另一个端点、另一把 key）。

# ② 一次性 env（显式给模型名）
YUSHAN_API_BASE=https://open.bigmodel.cn/api/paas/v4 \
YUSHAN_API_KEY=sk-... \
YUSHAN_MODEL=glm-5.3 \
  cargo run -p ys-coding-agent -- -p "任务"
```

env 直连的已知限制（见「未做」）：`provider` 恒为 `None` ⇒ `current_compat()` 回落到
`standard()`，因此该路径**不发 `reasoning_effort`**、也不解析 `reasoning_content`。
需要思考档位与思考显示时用 `/login`。

## 测试

| 文件 | 用例 | 钉住的东西 |
|---|---|---|
| `adapters/.../compat.rs` | `glm_capabilities` | `glm()` 的四项能力位：`reasoning_content` / 流式 usage / 接受 `reasoning_effort` / `tool_calls` 仍在 `tool_calls` |
| 同上 | `effort_dialect_defaults_to_verbatim` | 默认方言是 `Verbatim` —— 把 `Glm` 设成默认会让未声明方言的 provider 静默收到折叠后的档位 |
| 同上 | `verbatim_passes_configured_value_through` | `Verbatim` 逐字透传（含 `" high "`）、`None` 仍为 `None`；未声明支持的 provider 任何模型都不发 |
| 同上 | `glm_5_3_folds_every_tier` / `glm_5_2_folds_every_tier` | 两张折叠表**逐档**（含 `off` 与 `turbo` 之类白名单外取值 ⇒ 不发） |
| 同上 | `glm_gates_effort_by_model_identity` | 无法解析的 id 一律 `false` + `None`（fail-closed）；可解析的 id（5.3 / 5.3-flash / 5.2）一律 `true` |
| 同上 | `glm_ids_below_5_2_pass_through_but_never_send_off` | `< (5,2)` 分支：合法档位原样透传，`off` 必先映射为 `minimal` |
| 同上 | `glm_never_emits_off_on_the_wire` | 跨代际锚点：`glm-5.3` / `glm-5.3-flash` / `glm-5.2` 的 `off` 都不会原样落 wire |
| 同上 | `glm_generation_parse_is_case_insensitive` | 代际解析（只测解析器健壮性，不代表本仓提供那些 id）：大小写、小版本后紧跟字符、省略 minor、前缀缺失 |
| 同上 | `constructors_pin_reasoning_effort_support` | 只有 `opencode-go` / `glm` 的能力位为 `true`；`standard` / `deepseek` / `minimax` 为 `false` |
| `apps/.../provider.rs` | `test_glm_provider_metadata` | 两个端点的 base 逐字；方言；`session_header: None` |
| 同上 | `test_seed_catalog_only_for_glm_providers` | 种子只有 `glm` / `glm-coding`，顺序即起始模型；其余 provider 无种子 |
| 同上 | `test_glm_seed_preheats_catalog_without_login` | **未登录、零请求**时目录与窗口（1M）已可用 |
| 同上 | `test_real_catalog_overrides_glm_seed` | 真实目录**整体覆盖**种子（非合并）；只影响同 base，另一端点的种子不受影响 |
| 同上 | `test_fetch_targets_skips_seed_providers` | 已登录的 `glm` / `glm-coding` 不进拉取目标，`deepseek` 照常进 |
| 同上 | `test_fetch_targets_skips_env_direct_on_seed_base` | env 直连到种子 base（含尾斜杠写法）同样跳过；未知 base 照常拉 |
| `apps/.../app_loop.rs` | `login_with_seed_catalog_skips_network_and_fills_candidates` | `/login glm` 不走网络（耗时有上限断言），起始模型 `glm-5.3`，同一次刷新的 `View` 里已有两条候选 |
| 同上 | `model_changed_records_wire_effort` | 日志记 wire 值：`glm-5.3` 的 `xhigh` → `"max"`、`glm-5`（`< (5,2)`）→ `"xhigh"`、无法解析的代际 → `null`、`opencode-go` → `"xhigh"`（折叠不外溢） |
| `apps/.../config.rs` | `test_accepts_reasoning_effort_is_model_level` / `..._differs_from_provider_level` | 模型级门控与 provider 级能力位**必须可分**（provider 认字段 + id 无法解析：一真一假） |
| `apps/.../capabilities.rs` | `set_effort_warns_when_model_rejects_effort` | 无法解析的 id 有 warning（文案含 provider **与 model**）且设置照常保存；`glm-5.3` / `glm-5.2` / `opencode-go` 无 warning |
| `apps/.../main.rs` | `reasoning_effort_is_gated_by_provider_compat` | 两层门：能力位关 ⇒ 任何模型都不发；`glm` 逐档折叠（`< (5,2)` 分支含原样透传与 `off` 映射）；无法解析 ⇒ `None` |
| 同上 | `run_start_records_wire_reasoning_effort` | `gen_ai.request.reasoning_effort` 记折叠后的值（`max` / `null` / `xhigh`） |
| `apps/.../view.rs` | `test_build_view_glm_seed_catalog_enters_model_candidates` | 已登录后种子进 `/model` 候选、两个端点同名声各占一行、窗口 1M |
| 同上 | `test_build_view_defaults_without_credentials` | 内置 provider 计数为 6，`glm` / `glm-coding` 有内置 base |

**变异点**（各用例注释里标注的注入位置与预期变红项，本次文档核对**未重跑**）：

- 去掉 `ProviderRegistry::new()` 的种子写入 ⇒ `test_glm_seed_preheats_catalog_without_login`
  与 `test_build_view_glm_seed_catalog_enters_model_candidates` 变红。
- 去掉 `fetch_targets` 的两处 `Seed` 跳过 ⇒ `test_fetch_targets_skips_seed_providers` /
  `..._skips_env_direct_on_seed_base` 变红。
- 把 `accepts_reasoning_effort()` 换回 `supports_reasoning_effort` ⇒ config 两条用例与
  `set_effort_warns_when_model_rejects_effort` 变红。
- 把 `effort_for(&config.model, ...)` 换回原始配置值 ⇒ `model_changed_records_wire_effort`
  与 `run_start_records_wire_reasoning_effort` 变红。
- 把 `Glm` 方言误用到所有 provider ⇒ `opencode_go` 的 `xhigh` 锚点变成 `max`，适配器与
  `main.rs` 的透传断言同时变红。
- 删掉种子 `/login` 分支（一律走 `fetch_models`）⇒ `login_with_seed_catalog_skips_network_...`
  变红。

## 未做

- **不做 `/v1/models` 的猜测性探测**：路径已确定（`build_models_url` 在非 `/v1` 结尾的 base 后追加
  `/v1/models`），不需要试错；拿不到（如编码套餐端点 404）就回落到种子。
- **种子只含当代两条**：种子（`glm-5.3` / `glm-5.3-flash`）仅用于拿不到目录的端点。
  有目录时模型名来自真实目录，其**前 3 项**进 `/model` 候选（`catalog_limit = Some(3)`）。
  用户仍可在 `config.json` / `/model` 里手敲任何名字：能解析出 GLM 代际的按该代际的折叠表
  发 `reasoning_effort`；无法解析的则**不发**（门控 fail-closed），且 `/effort` 会给一条 warning。
- **env 直连不认 provider 方言**：`YUSHAN_API_BASE` 指向 GLM 时 `provider` 恒为 `None`，
  `current_compat()` 回落到 `standard()` ⇒ 不发 `reasoning_effort`、不解析
  `reasoning_content`（思考内容不显示）。
- **启动期「模型名缺失」路径**：env 直连且未给 `YUSHAN_MODEL` 时，会向端点发一次 `/v1/models`
  请求并取第一条作起始模型（失败则交互式降级、一次性模式硬失败）。种子**不**参与该路径 ——
  它只在 `/login` 与后台刷取的回落里生效。
- **不设输出上限**：`max_tokens` 一律不序列化，128K 最大输出由厂商侧执行。
- **不做模型目录的协议过滤**：适配器只打 `/chat/completions`；端点若上线别的协议模型，
  选中即 400，靠 `/model` 换一条。
