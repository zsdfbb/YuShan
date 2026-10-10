# OpenCode Go 接入 — As-built

> 性质：**实际建成记录**。相关设计见 `docs/arch/commands/login-model-behavior/design.md`
> （Provider 注册表），重试/超时见 ADR-0028。日期：2026-10-10（默认模型修订：同日）

## 一句话

新增内置 provider `opencode-go`（`https://opencode.ai/zen/go/v1`，默认模型
`deepseek-flash`），并给适配器补上**额外请求头**这一个传输层能力 —— 网关要求每个
模型请求带 `x-opencode-session`，缺了直接 `400 MissingSessionID`。

## 网关契约（实测，非推测）

| 维度 | 行为 |
|---|---|
| 认证 | `Authorization: Bearer <key>`（与 deepseek/minimax 相同） |
| **会话头** | 必须有非空的 `x-opencode-session`，否则 400；值只要求**一段对话内稳定** |
| 模型目录 | `GET /v1/models`（**不需要**会话头，返回 OpenAI 形状） |
| 协议混杂 | 不同模型挂不同端点：`/chat/completions`、`/responses`、`/messages`（Anthropic 方言） |
| 思考字段 | 推理型模型回 `reasoning_content`（DeepSeek 方言），非 `reasoning` |
| usage | 放行 `stream_options.include_usage` |

## 落点

```
adapters/model-openai-compatible/src/lib.rs      OpenAICompatibleModel::with_extra_header + send() 注入
adapters/model-openai-compatible/src/compat.rs   ProviderCompat::opencode_go()
apps/coding-agent/src/provider.rs                ProviderInfo.session_header + request_headers() + session_id()
apps/coding-agent/src/config.rs                  Config::request_headers()
apps/coding-agent/src/main.rs                    两个 model factory 逐头 with_extra_header
```

## 关键行为（实现期定死，勿凭直觉改）

**会话头的两条判定路径**（`ProviderRegistry::request_headers`）：① 按 **provider 名**；
② 按 **api_base 与内置 provider 的 base 相等**（尾斜杠不计）。② 是为 env 直连准备的 ——
那条路径（`YUSHAN_API_BASE`）`provider` **恒为 `None`**，只做 ① 会让 env 用户每次请求
都吃 400。变异自检：删掉 ② 后 `test_request_headers_by_api_base_for_env_path` 变红。

**头值是进程级稳定 id**（`session_id()`：`ys-<pid>-<秒>_<纳秒>`，`OnceLock` 生成一次），
不是会话级：模型的构造点（`Config::build_model`）拿不到 `Wiring` 里的会话 id（启动期
model 先于 wiring）。网关只要求对话内稳定，故同一进程内 `/new`、`/resume` 复用同一 id ——
影响的是路由与 prompt cache 亲和，不影响正确性。**同一 id 在进程间必须可能不同**，
否则多实例会互相污染缓存亲和。

**只有 `chat/completions` 那批模型可用**：适配器只会打 `/chat/completions`。网关目录
里 `/responses`（`grok-*`、`gpt-*-luna`、`muse-spark-*`）与 `/messages`
（`claude-haiku-5-5`、`minimax-m3`/`m2.7`、`qwen3.8-*`、`qwen3.7-plus`）的模型会在
`/model` 候选里出现，选中即 `400 ModelProtocolUnsupported`。目录**不做过滤** ——
静态白名单必然过时；这条限制写在这里，不写进代码。

**协议归属不按模型名猜**：`deepseek-flash` 一度被当作 `/responses` 模型，实测它在
`/chat/completions` 上正常返回（终包含 `reasoning_content`）—— 名字带 `gpt-*`/`claude-*`
不必然走别的端点，反之亦然。判定只认实测，故默认模型取 `deepseek-flash`（目录里另有
`deepseek-v4-flash`、`deepseek-v4.1-flash` 等近名条目）。

**context window 刻意留空**：网关不返回窗口，官方文档也没有 go 模型的窗口表，
故 `builtin_context_window` 不收录 —— 宁可回退 128_000（提前压缩），也不编造一个可能
让上下文溢出的值。需要更大窗口的模型（如 `deepseek-v4-pro`）请写 `~/.yushan/config.json` 覆盖。

## 两条配置路径（都已实测跑通）

```bash
# ① 持久（推荐）：写 ~/.yushan/auth.json（0o600）与 state.json，之后直接起 TUI
# ② 一次性 env
YUSHAN_API_BASE=https://opencode.ai/zen/go/v1 YUSHAN_API_KEY=oc_sk_... YUSHAN_MODEL=deepseek-flash \
  cargo run -p ys-coding-agent -- -p "任务"
```

## 未做

- 不做 `/responses` 与 `/messages` 两种协议（要新适配器，属另一刀）。
- 不做模型目录的协议过滤（见上）。
- 不按会话切 id（见上）。
