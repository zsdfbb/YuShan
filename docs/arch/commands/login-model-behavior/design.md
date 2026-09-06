# /login & /model 命令行为改进 — 设计方案

## 概述

对照 Pi 项目，改进 YuShan 的 `/login` 和 `/model` 命令行为，补齐 5 个关键差距：凭证持久化、模型列表动态拉取、Provider-aware compat、模型选择器增强、/logout 修复。

## 调研发现

### Pi 的实现方式

| 维度 | Pi 的做法 |
|------|----------|
| 模型列表 | 静态 catalog（build-time 生成 300+ 模型）+ `models.json` 自定义 + 扩展可通过 `/v1/models` 动态注册 |
| 凭证持久化 | `~/.pi/agent/auth.json`（0o600），支持 API Key + OAuth，优先级：CLI flag > auth.json > env var |
| Provider-aware | 4 种 wire protocol（openai-completions / anthropic-messages / google-generative-ai / bedrock）+ 20+ compat flags |
| 模型选择器 | 可搜索列表 + 当前模型高亮 + scoped favorites + Ctrl+P 循环切换 |

### 各 Provider `/v1/models` 端点实测结论

| Provider | 端点路径 | 认证 | 响应格式 | 注意事项 |
|----------|---------|------|---------|---------|
| DeepSeek | `{base}/v1/models` | Bearer | 标准（仅 id/object/owned_by） | 最精简，无 created 字段 |
| MiniMax | `{base}/v1/models` | Bearer | 标准 | PascalCase 模型 ID |
| Ollama | `{base}/v1/models` | Bearer（忽略） | 标准 | auth 可填任意值 |
| Groq | `{base}/models` | Bearer | 标准 | base URL 已含 `/v1` |
| OpenRouter | `{base}/models` | Bearer | 非标准（无 `object:"list"`） | 有 pricing/context_length 等丰富字段 |

**关键结论**：所有 provider 都返回 `data` 数组，`id` 字段可靠。路径构造需注意 base URL 是否已含 `/v1`。

## 候选方案

### 方案 A：最小改动（未选）

最小 diff：在现有 `Config` 上加 `dynamic_models` 字段，命令内联所有逻辑。

- **优点**：~65 行新代码，0 新模块
- **缺点**：`Provider` 保持 `&'static str`，Config 既是配置又是运行时缓存，`ModelCommand` 膨胀

### 方案 B：Provider 注册表（推荐）

引入 `ProviderRegistry`，集中管理 provider 知识、凭证持久化、模型获取、compat 选择。

- **优点**：清晰分离，消除 `&'static str` 约束，扩展性好
- **缺点**：~230 行新代码，1 个新模块

### 方案 C：分层配置（未选）

拆分为 `AuthLayer` + `ModelLayer` + `CompatLayer` 三个独立模块。

- **优点**：最大可测试性
- **缺点**：~285 行新代码，3 个新模块，对 3 个 provider 过度设计

## 推荐方案：B（Provider 注册表）的务实简化版

保持方案 B 的核心思路（引入 `ProviderRegistry`），但根据 MVP 范围做以下简化：

1. **不引入 `CommandContext` 变更** — registry 作为 `Config` 的内部字段，命令通过 `ctx.config.registry` 访问
2. **不支持多 provider 同时登录** — registry 只跟踪当前 provider
3. **动态模型缓存** — 内存缓存，不持久化到文件

### 模块结构

```
apps/coding-agent/src/
├── config.rs              # Config struct（持有 registry 引用）
├── provider.rs            # NEW: ProviderRegistry + ProviderInfo + AuthEntry
├── commands/
│   ├── mod.rs             # CommandContext 不变
│   └── builtin.rs         # LoginCommand/LogoutCommand/ModelCommand 使用 registry
└── main.rs                # 创建 registry，传入 Config
```

### 核心数据结构

```rust
// provider.rs

/// Provider 完整信息（owned String，支持动态扩展）
pub struct ProviderInfo {
    pub name: String,
    pub api_base: String,
    pub default_model: String,
    pub compat: ProviderCompat,
}

/// 凭证存储条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthEntry {
    pub api_base: String,
    pub api_key: String,
    pub model: String,
}

/// Provider 注册表：集中管理 provider 知识和凭证
pub struct ProviderRegistry {
    /// 内置 provider 列表
    providers: Vec<ProviderInfo>,
    /// 凭证存储（provider name → entry）
    auth_store: AuthStore,
    /// 动态模型缓存（api_base → models）
    model_cache: HashMap<String, Vec<ApiModel>>,
}
```

```rust
// config.rs

pub struct Config {
    pub api_base: Option<String>,
    pub api_key: Option<String>,
    pub model: String,
    pub cwd: PathBuf,
    pub provider: Option<String>,
    model_factory: Option<ModelFactory>,
}
// 注册表通过 main.rs 创建后传入 CommandContext，
// 或作为 Config 的 pub 字段（待定）
```

### 关键数据流

**/login（无参数）：**
```
registry.providers()           → 返回内置 provider 列表
inquire::Select                → 选择 provider
registry.save_auth(name, entry) → 写入 ~/.yushan/auth.json
registry.fetch_models(base, key) → GET {base}/models → 缓存结果
registry.current_compat(name)  → 返回 ProviderCompat
Config 设置 api_base/api_key/model/provider
build_model()                  → 使用 current_compat 构建
Agent.set_model()
```

**/model（无参数）：**
```
registry.available_models(base, key) → 先查缓存，miss 则 fetch，失败 fallback known_models()
构建标签列表：当前模型标记 ✓，按 provider 过滤
inquire::Select → 选择
Config 设置 model → build_model() → Agent.set_model()
```

**/logout：**
```
registry.remove_auth(provider_name) → 从 auth.json 删除
Config 清除 api_base/api_key/provider
Agent.set_model(None)
```

**启动恢复：**
```
Config::from_env() → 读取 env vars
registry.load_auth() → 读取 ~/.yushan/auth.json
匹配 api_base 或取第一个 → 恢复 api_key/provider/model
factory 使用 current_compat() 构建模型
```

### 文件变更清单

| 文件 | 变更 | 预估行数 |
|------|------|---------|
| `provider.rs`（新建） | ProviderRegistry + ProviderInfo + AuthEntry + fetch_models + auth 持久化 | ~200 行 |
| `config.rs` | 移除 known_providers/known_models/AuthEntry/AuthStore/fetch_models，Provider 改为引用 ProviderInfo | ~-150 行 |
| `commands/builtin.rs` | LoginCommand/LogoutCommand/ModelCommand 使用 registry | ~+50 行 |
| `commands/mod.rs` | CommandContext 增加 registry 字段（或通过 Config 间接访问） | ~+5 行 |
| `main.rs` | 创建 ProviderRegistry，传入 CommandContext | ~+10 行 |
| `Cargo.toml` | 添加 reqwest 依赖 | ~+2 行 |

### auth.json 格式

```json
{
  "deepseek": {
    "api_base": "https://api.deepseek.com",
    "api_key": "sk-xxx",
    "model": "deepseek-chat"
  }
}
```

- 路径：`~/.yushan/auth.json`
- 权限：0o600
- 格式：`{ provider_name: AuthEntry }`
- 加密：不加密（与 Pi 一致）

### fetch_models 策略

```rust
pub async fn fetch_models(api_base: &str, api_key: &str) -> Result<Vec<ApiModel>, String> {
    // 1. 构造 URL：处理 base URL 是否已含 /v1
    let url = if api_base.ends_with("/v1") || api_base.ends_with("/v1/") {
        format!("{}/models", api_base.trim_end_matches('/'))
    } else {
        format!("{}/v1/models", api_base.trim_end_matches('/'))
    };

    // 2. GET + Bearer auth，5s 超时
    // 3. 解析 { "data": [{ "id": "...", "owned_by": "..." }] }
    // 4. 只提取 id 和 owned_by，忽略其他字段
}
```

**Fallback 策略**：
- API 返回成功 → 使用 API 列表
- API 失败（超时/错误）→ 使用 `known_models()` 静态列表 + 标注"离线模式"
- 无凭证 → 只显示静态列表

## 未澄清问题

- [ ] `CommandContext` 是否直接持有 `&mut ProviderRegistry`，还是通过 `&mut Config` 间接访问？→ 建议直接持有，更清晰
- [ ] `/model deepseek-reasoner` 直接指定时，是否校验模型在可用列表中？→ 建议不校验（保持当前行为），仅提示
- [ ] `ProviderInfo` 的 compat 是静态映射还是可配置？→ MVP 静态映射（name → compat），后续可通过 models.json 扩展

## 后续建议

- 建议用 `prototype` 验证 DeepSeek 和 MiniMax 的 `/v1/models` 端点实际返回（用户已计划）
- 后续迭代：多 provider 同时登录、OAuth 支持、默认模型持久化
