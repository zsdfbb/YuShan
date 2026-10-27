# 小米 MiMo 接入（`mimo`）— As-built

> 性质：**实际建成记录**。provider 注册表与 `/login` 的既有契约见
> `docs/design-final/opencode-go.md` 与 `docs/arch/commands/login-model-behavior/design.md`；
> `reasoning_effort` 的规范、门控与持久化见 `docs/design-final/effort.md`。
> 同族 provider 的接入先例见 `docs/design-final/glm.md`。日期：2026-10-27

## 一句话

新增一个内置 provider —— `mimo`（端点 `https://api.xiaomimimo.com/v1`）—— 并为适配器
补上**按该端点合法档位集合折叠** `reasoning_effort` 的能力。与既有 provider 的两个关键差别：

① 该端点的 `/v1/models` 把**语音模型排在最前**，而本仓只跟 chat 端点对话 ⇒ 目录必须**过滤**；
② `reasoning_effort` 的合法取值是该端点自己的子集（`none|low|medium|high|max`），
`minimal` / `xhigh` / `off` 直接 400 ⇒ 必须本地折叠。

## 厂商契约（外部依据）

以下全部为**实测**（2026-10-27，端点 `https://api.xiaomimimo.com/v1`）：

| 维度 | 行为 |
|---|---|
| 端点 | `https://api.xiaomimimo.com/v1`（请求打 `<base>/chat/completions`） |
| 认证 | `Authorization: Bearer <key>`；**无**额外会话头 |
| 模型目录 | `GET /v1/models` → **200，7 条**，字段只有 `id` / `object` / `owned_by`；**不报窗口**（无 `context_window` 也无 `context_length`）。`GET /v1/models/<id>` → 400（`404 NOT_FOUND`）；`?type=chat` / `?limit=N` 两个查询参数都被**忽略**（返回同样的 7 条）⇒ 端点**不提供**按能力类型筛选的手段 |
| 目录顺序 | 语音模型在前：`mimo-v2.5-asr` / `mimo-v2.5-tts` / `mimo-v2.5-tts-voiceclone` / `mimo-v2.5-tts-voicedesign`，之后才是 chat 模型 `mimo-v2.6-flash` / `mimo-v2.6-pro` / `mimo-v2.6-pro-ultraspeed` |
| 语音模型 | 在 `/chat/completions` 上**必然 400**：`asr` → `ASR request requires a user message with input_audio content`；`tts` 家族 → `messages must contain an assistant role for TTS model` |
| 流式 | SSE，以 `data: [DONE]` 收尾；**末包**带 `usage`（含 `prompt_tokens_details.cached_tokens` 与 `completion_tokens_details.reasoning_tokens`）。实测接受 `stream_options.include_usage` |
| 思考字段 | `delta.reasoning_content` 是思考增量（与 DeepSeek 同形，非 `reasoning`） |
| `tool_calls` | 在 `tool_calls` 字段里（**不是** text） |
| `reasoning_effort` | 接受顶层字段。合法集实测为 **`none` / `low` / `medium` / `high` / `max`**；`minimal` / `xhigh` / `off` 返回 **400**（`Param Incorrect`，`param` 写 `must be one of 'none', 'low', 'medium' or 'high'`）。⚠️ **错误文案漏了 `max`** —— 实测 `max` 返回 200，故以实测为准，**不**按文案把 `max` 也排除 |
| 上下文窗口 | **1,048,576**。实测 `prompt_tokens` 在超窗时**钉在 1,048,570**（= 1,048,576 − 6），**不报错** ⇒ 该端点是**静默截断**，不是拒绝 |

## 落点

| 文件 | 改动点 |
|---|---|
| `adapters/model-openai-compatible/src/compat.rs` | `EffortDialect { Verbatim, Glm, Mimo }`；`ProviderCompat.effort_dialect`；`ProviderCompat::mimo()`；`MIMO_AUDIO_TOKENS` / `MIMO_DISABLED_TOKENS` / `id_has_token` / `is_mimo_audio_model` / `is_mimo_disabled_model` / `is_mimo_chat_model`；`effort_for` 的 `Mimo` 折叠分支 |
| `apps/coding-agent/src/provider.rs` | `ProviderInfo.catalog_exclude`；`MIMO_EXCLUDED_TOKENS`；`mimo` 内置项；`ProviderRegistry::filter_catalog` / `catalog_exclude_for_base`；`apply_model_cache` 写入前过滤；`KNOWN_MODEL_WINDOWS` 补 MiMo 两条 |
| `apps/coding-agent/src/app_loop.rs` | `/login` 在**选起始模型之前**过滤目录 |
| `apps/coding-agent/src/main.rs` | 启动期 `adopt_first_catalog_model` 在取第一条之前过滤 |
| `apps/coding-agent/src/view.rs` | 内置 provider 计数 6 → 7；`mimo` 进 `/login` 选择器 |

## 关键行为（实现期定死，勿凭直觉改）

**为什么目录必须过滤，而不是只在展示层过滤**：该端点的目录**首条就是语音模型**
（`mimo-v2.5-asr`），而 `/login` 的起始模型取**目录第一条**。只在 `/model` 展示层过滤的话，
登录后当前模型仍是一个在 chat 端点上必然 400 的语音模型 ⇒ 登录即不可用。
故过滤作用在**两处**：

- **目录进缓存前**（[`ProviderRegistry::apply_model_cache`]）—— 缓存里不该出现用不了的模型，
  否则 `/model` 候选、窗口查询、归属反查都会看到它们；
- **选起始模型前**（`/login` 与启动期 `adopt_first_catalog_model`）—— 保证起始模型可用。

**为什么按「能力类型」排除（`asr` / `tts`）而不是按白名单保留 `mimo-v2.6-`**：
白名单会把未来新增的 chat 模型（如 `mimo-v3-`）一并误杀；排除式能同时挡掉未来新增的
语音类型。端点**不提供**按能力筛选的手段（`?type=chat` 被忽略），故只能在本地按 id 判定。

**匹配规则是「`-` 分段精确等于」，不是子串包含**：子串匹配会被无关的连续字母误伤
（如 `mimo-v2.6-classify` 里的 `asr`）。判定逻辑落在适配器的
[`id_has_token`]，**provider 侧与方言侧共用同一份** —— 两处各写一遍必然漂移。

**`ultraspeed` 是用户明确禁用的模型**，与语音模型**分两类**记
（[`MIMO_DISABLED_TOKENS`] vs [`MIMO_AUDIO_TOKENS`]）：前者是「不用于本仓」，
后者是「不是 chat 模型」。两者都不进目录、都不收 `reasoning_effort`。

**MiMo 折叠表**（`effort_for` 对 `EffortDialect::Mimo` 的唯一实现）：

| 配置值 | MiMo wire | 说明 |
|---|---|---|
| `off` | `low` | `off` 不是合法 wire 值；映射到**最低档**而非 `none` —— `none` 是「关闭思考」，与 `off`（本仓语义：最低思考强度）不同 |
| `minimal` | `low` | `minimal` 不是合法 wire 值 |
| `low` | `low` | 原样 |
| `medium` | `medium` | 原样 |
| `high` | `high` | 原样 |
| `xhigh` | `max` | `xhigh` 不是合法 wire 值 |
| `max` | `max` | 原样（**尽管错误文案没列 `max`** —— 实测接受） |

- 白名单之外的取值（上游配置错误）**不猜映射**，直接不发 —— 与其发一个必然 400 的值，
  不如退化为「不带该字段」。
- **模型级门控**：id 必须能识别为 MiMo 的 chat 模型（以 `mimo-` 开头**且**前缀之后有实际
  模型名**且**不是语音模型**且**不是禁用模型），否则 **fail-closed**（不发）。
  识别不出 MiMo 模型就不发 —— 与 GLM 方言同一原则。

**窗口走 ③ 级兜底表**：该端点的目录**不报窗口**（实测字段里没有 `context_window` /
`context_length`），故 `mimo-v2.6-pro` 与 `mimo-v2.6-flash` 的窗口取自
[`KNOWN_MODEL_WINDOWS`]（1,048,576，实测值）。**不是**兜底默认值的 1,000,000 ——
两者差 4.8%，且该端点是静默截断，窗口报小一点才安全。

**⚠️ 静默截断的后果**：超窗时端点**不报错**，而是把 prompt 砍到 1,048,570 token。
本仓的压缩触发点是 `window × 0.85` ≈ 891k < 1,048,570 ⇒ 正常路径在触窗**之前**就压缩了。
但**一次性模式**（`-p` / `--json`）不压缩，超长输入会被端点静默截断 ⇒
**开头的内容会丢**（实测：超长 prompt 的回答为空、`finish_reason: length`）。
这是端点行为，不是本仓的 bug，但值得知道。

## 两条配置路径

```bash
# ① 持久（推荐）：TUI 里 /login mimo → 填 key；起始模型取**过滤后**的目录第一条
#    （`mimo-v2.6-flash`，实测目录第一条是语音模型 `mimo-v2.5-asr`），
#    /model 候选只剩 chat 模型两条（`mimo-v2.6-flash` / `mimo-v2.6-pro`）。

# ② 一次性 env（显式给模型名）
YUSHAN_API_BASE=https://api.xiaomimimo.com/v1 \
YUSHAN_API_KEY=sk-... \
YUSHAN_MODEL=mimo-v2.6-pro \
  cargo run -p ys-coding-agent -- -p "任务"
```

env 直连的已知限制（与 GLM 同）：`provider` 恒为 `None` ⇒ `current_compat()` 回落到
`standard()`，因此该路径**不发 `reasoning_effort`**、也不解析 `reasoning_content`。
需要思考档位与思考显示时用 `/login`。

## 测试

| 文件 | 用例 | 钉住的东西 |
|---|---|---|
| `adapters/.../compat.rs` | `mimo_capabilities` | `mimo()` 的四项能力位：`reasoning_content` / 流式 usage / 接受 `reasoning_effort` / `tool_calls` 仍在 `tool_calls` |
| 同上 | `mimo_folds_every_tier` | 折叠表**逐档**（含白名单外取值 ⇒ 不发） |
| 同上 | `mimo_gates_effort_by_model_identity` | 语音模型 / 禁用模型 / 识别不出 MiMo ⇒ 一律 fail-closed |
| 同上 | `mimo_audio_detection_is_segment_exact_and_case_insensitive` | 分段精确匹配、大小写不敏感；音频与禁用**两类分开**判 |
| 同上 | `mimo_never_emits_illegal_tiers_on_the_wire` | 跨档锚点：wire 值永远不出现 `minimal` / `xhigh` / `off` |
| 同上 | `constructors_pin_reasoning_effort_support` / `effort_dialect_defaults_to_verbatim` | `mimo` 的能力位为 `true`；默认方言仍是 `Verbatim` |
| `apps/.../provider.rs` | `test_mimo_provider_metadata` | base 逐字；方言；`session_header: None`；`catalog_limit: None` |
| 同上 | `test_mimo_catalog_excludes_unusable_models` | 语音模型与 `ultraspeed` 不进目录 |
| 同上 | `test_mimo_filter_runs_before_picking_the_first_model` | 过滤发生在「取第一条」**之前** |
| 同上 | `test_catalog_exclude_only_applies_to_mimo` | 过滤规则只挂 MiMo，别家不过滤 |
| `apps/.../view.rs` | `test_build_view_defaults_without_credentials` | 内置 provider 计数为 7，`mimo` 有内置 base |

**变异点**（实测注入 ⇒ 预期变红，本次**已实跑**）：

| 注入 | 变红的用例 |
|---|---|
| MiMo 折叠表把 `off` 映射成 `none` | `mimo_folds_every_tier` |
| 模型级门控去掉音频 / 禁用判定（fail-open） | `mimo_gates_effort_by_model_identity` + `mimo_audio_detection_is_segment_exact_and_case_insensitive` |
| `apply_model_cache` 不再过滤 | `test_mimo_catalog_excludes_unusable_models` |
| 排除词改成整串（`mimo-v2.5-asr` 而非分段 `asr`） | `test_mimo_filter_runs_before_picking_the_first_model` + `test_mimo_catalog_excludes_unusable_models` |

## 未做

- **不做「模型目录的协议过滤」**（GLM 那条限制同样适用）：适配器只打 `/chat/completions`。
  本仓按**能力类型**（语音 / 禁用）过滤，而不是按协议枚举 —— 端点不报协议字段。
- **不设输出上限**：`max_tokens` 一律不序列化，输出上限由厂商侧执行。
- **不为 MiMo 建种子目录**：该端点的 `/v1/models` 可用（实测 200），目录是第一手来源；
  写死种子反而会随厂商升版过时（且要维护窗口值）。
- **不做端到端验证**（本轮）：实测消耗的 token 触发了该 key 的余额上限（402
  `Insufficient account balance`），端到端用例待充值后再跑。
