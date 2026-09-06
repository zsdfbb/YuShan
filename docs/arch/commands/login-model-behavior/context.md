# /login & /model 命令行为改进 — 架构上下文

## 概述

对照 Pi 项目分析当前 `/login` 和 `/model` 命令的行为差距，确定 MVP 需要的最小改进集。

## 现有实现

### 模块边界

```
main.rs:  设置 model_factory 闭包（硬编码 ProviderCompat::standard()）
          ↓
Config:   单一 api_base / api_key / model（内存）
          ↓
commands/builtin.rs:  LoginCommand / ModelCommand / LogoutCommand
          ↓
agent-runtime/Agent:  set_model() 热替换
          ↓
model-openai-compatible:  HTTP 请求 → LLM API
```

### 核心抽象

| 结构体 | 位置 | 职责 |
|--------|------|------|
| `Config` | config.rs:55 | 存 api_base / api_key / model + model_factory |
| `Provider` | config.rs:10 | name / api_base / default_model（静态） |
| `KnownModel` | config.rs:18 | provider / model_id / display（静态） |
| `LoginCommand` | builtin.rs:85 | 交互式选择 provider → 输入 key → 构建模型 |
| `ModelCommand` | builtin.rs:284 | 交互式选模型或直接 `/model xxx` → 构建模型 |

### 关键数据流

```
/login (无参数)
  → known_providers() [硬编码 3 个]
  → inquire::Select 选择 provider
  → 如是 custom: 询问 api_base
  → 询问 api_key
  → Config 设置 api_base/api_key/model
  → build_model() → OpenAICompatibleModel(ProviderCompat::standard())
  → Agent.set_model()

/model (无参数)
  → known_models() [硬编码 3 个]
  → inquire::Select 选择
  → Config 设置 model
  → build_model() → 重建模型
  → Agent.set_model()

/model deepseek-reasoner
  → 直接 Config 设置 model = "deepseek-reasoner"
  → build_model() → 重建模型（不校验模型名）
```

### 外部依赖

| 依赖 | 用途 | 备注 |
|------|------|------|
| `inquire` | 交互式终端选择器 | 支持箭头键 + 输入过滤，不支持 fuzzy |
| `openai-compatible` adapter | HTTP 调用 LLM API | 内部 crate |

## 对照 Pi 的差距分析

### 差距 1：模型列表硬编码

| 维度 | Pi | YuShan（当前） |
|------|-----|---------------|
| 模型列表来源 | API `GET /v1/models` 动态拉取 + 缓存 | 硬编码 `known_models()`（3 个） |
| 列表准确性 | 实时反映 provider 可用模型 | 与任何 provider 实际可用模型不匹配 |
| 选择 UI | fuzzy 搜索 + 当前模型高亮 + provider 标签 | `inquire::Select` 基本列表 |

**影响**：用户看到的模型可能在当前 provider 上不可用；也无法发现 provider 新增的模型。

### 差距 2：凭证不持久化

| 维度 | Pi | YuShan（当前） |
|------|-----|---------------|
| 持久化 | `~/.pi/agent/auth.json`（0o600） | 仅内存，重启丢失 |
| 启动恢复 | 自动加载已有凭证 | 仅从 env 变量 |
| 登录状态检测 | Provider 旁显示 ✓ | 无检测 |

**影响**：每次启动都需要重新 `/login`，用户体验差。

### 差距 3：ProviderCompat 未生效

| 维度 | Pi | YuShan（当前） |
|------|-----|---------------|
| compat 选择 | 根据 provider 动态设置 | 所有 provider 统一 `standard()` |
| deepseek reasoning_content | 正确处理 | 存在 `deepseek()` preset 但未使用 |
| minimax tool_calls 格式 | 正确处理 | 存在 `minimax()` preset 但未使用 |

**影响**：DeepSeek R1 的推理内容、MiniMax 的工具调用格式无法正确处理。

### 差距 4：Provider 感知缺失

| 维度 | Pi | YuShan（当前） |
|------|-----|---------------|
| 模型列表作用域 | 当前 provider 的模型 | 所有 provider 混合 |
| 切换 provider | 凭证独立存储，自动匹配 | 需重新 /login |
| 多 provider 同时登录 | ✅ | ❌ 单一 api_base/api_key |

**影响**：选了 MiniMax 的模型但用 DeepSeek 的 key 调用，静默失败。

### 差距 5：/logout 不彻底

**现状**：`/logout` 只调用 `Agent.set_model(None)`，不清除 `Config` 中的 api_base/api_key。
**影响**：登录状态逻辑不一致——agent 认为未登录，但 config 仍有凭证。

## 约束

- **技术**：`Provider` 和 `KnownModel` 使用 `&'static str`，改为动态需要调整生命周期或改用 `String`
- **演进**：凭证持久化格式需考虑向后兼容（多 provider 存储扩展）
- **组织**：当前只有 3 个 provider，扩展性需求不高

## 需求范围

### 范围内（MVP 改进）

1. **模型列表从 API 动态拉取**
   - `fetch_models(api_base, api_key) -> Vec<ModelInfo>` 函数
   - 调用 `GET {api_base}/models`，解析 `{ "data": [{ "id": "...", "owned_by": "..." }] }`
   - `/model` 优先使用 API 返回列表，失败 fallback 到 `known_models()`
   - 拉取结果内存缓存（不持久化）

2. **凭证持久化到 `~/.yushan/auth.json`**
   - `/login` 成功后写入文件
   - 启动时 `Config::from_env()` 也检查 auth.json，env 优先
   - `/logout` 从 auth.json 删除对应 provider 的凭证
   - 文件权限 0o600

3. **Provider-aware ModelFactory**
   - `Provider` 增加 `compat` 字段（或 factory 根据 provider name 选择）
   - factory 闭包根据 provider 选择对应的 `ProviderCompat`
   - DeepSeek → `ProviderCompat::deepseek()`，MiniMax → `ProviderCompat::minimax()`

4. **/model 选择器增强**
   - 当前模型高亮标记（用 `>` 或 ✓）
   - 从 API 拉取时标注 provider 来源
   - provider 作用域过滤（仅显示当前 provider 的模型）

5. **修复 /logout**
   - 清除 Config 中的 api_base / api_key / model
   - Agent.set_model(None)

### 范围外（后续迭代）

- 多 provider 同时登录
- OAuth 认证方式
- 默认模型持久化（Ctrl+S 保存）
- Fuzzy 搜索（inquire 默认过滤已够用）
- 编辑器边框颜色等 UI 增强

### 关键场景

- **场景 1：首次启动 → /login**
  启动 → 无凭证 → /login → 选 provider → 输入 key → 自动拉取模型列表 → 成功 → 模型可用

- **场景 2：重启后自动恢复**
  启动 → 读取 auth.json → 恢复凭证 → 自动构建模型 → 可直接对话

- **场景 3：/model 切换模型**
  已登录 → /model → 显示当前 provider 的可用模型列表（从 API 获取）→ 选择 → 切换 → 会话保留

- **场景 4：/model 直接指定**
  已登录 → /model claude-sonnet-4 → 校验模型是否在可用列表中 → 是则切换，否则提示

- **场景 5：/logout → 重启**
  /logout → 凭证清除 → 重启 → auth.json 为空 → 需要重新 /login

- **场景 6：API 拉取失败 fallback**
  /model → API 超时 → 显示硬编码列表 + 标注"离线模式"

## 未澄清问题

- [ ] `GET /models` 端点：所有 OpenAI 兼容 provider 都支持？MiniMax 是否有差异？
- [ ] auth.json 是否需要加密？Pi 只做 0o600，建议 MVP 保持一致
- [ ] Provider 切换时（/login 新 provider）是否清除旧 provider 的 config？建议保留 auth.json 中的旧凭证，仅更新当前 config
- [ ] `fetch_models` 是否需要超时和重试？建议 5s 超时 + 1 次重试

## 后续建议

- 建议用 `arch-design` 做详细方案设计（API 接口、auth.json 格式、Provider 扩展方式）
- 建议用 `prototype` 验证 `GET /models` 在各 provider 上的返回格式差异
