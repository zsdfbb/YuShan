# /login & /model 行为改进 — 架构上下文

## 概述

对照 Pi 项目分析当前 /login 和 /model 的行为差距，确定 MVP 需要的最小改进集。

## 现有实现 vs Pi 对照

### /login 行为对比

| 维度 | Pi | YuShan（当前） |
|------|-----|---------------|
| Provider 来源 | 内置 catalog + models.json 覆盖 | 硬编码 `known_providers()`（3 个） |
| 选择 UI | fuzzy 搜索 + 箭头 + 状态标记 | `inquire::Select`（基本箭头选择） |
| 认证方式 | OAuth（浏览器）+ API Key 两种 | 仅 API Key |
| 多 provider 同时登录 | ✅ 每个 provider 独立存储 | ❌ 单一 api_base/api_key/model |
| 凭证持久化 | `~/.pi/agent/auth.json`（0o600） | ❌ 仅内存，重启丢失 |
| 登录后行为 | 自动选默认模型 + 刷新 catalog | 设置 config + 构建模型 |
| 登录前已有凭证 | 检测并显示 ✓ 标记 | 无检测 |

### /model 行为对比

| 维度 | Pi | YuShan（当前） |
|------|-----|---------------|
| 模型列表来源 | API 动态拉取（GET /models）+ 缓存 | 硬编码 `known_models()`（3 个） |
| 选择 UI | fuzzy 搜索 + 当前模型高亮 + provider 标签 | `inquire::Select`（基本列表） |
| 模型可用性 | 区分已认证/未认证 provider 的模型 | 不区分，全部显示 |
| 直接切换 | `/model claude-sonnet-4` 精确匹配 → 立即切换 | `/model xxx` 接受任意字符串 |
| 默认模型 | Ctrl+S 保存为默认，启动时自动加载 | 无默认模型概念 |
| 模型切换副作用 | 更新 footer、编辑器边框颜色 | 仅更新 config + 重建模型 |
| 切换时 session | 保留 | 保留 |

## 关键差距分析

### 差距 1：模型列表硬编码（高优先级）

**现状**：`known_models()` 返回 3 个硬编码模型，不匹配任何 provider 的实际可用模型。

**Pi 的做法**：从 API `GET /v1/models` 端点动态拉取模型列表，缓存在 `models-store.json`。大多数 OpenAI 兼容 provider 都支持此端点。

**建议**：
- `/model` 无参数时，先尝试从当前 provider 的 API 拉取模型列表
- 拉取失败时 fallback 到静态列表
- 拉取成功后缓存结果

### 差距 2：凭证不持久化（高优先级）

**现状**：`/login` 设置的凭证只存在 `Config` 内存中，重启后丢失。

**Pi 的做法**：保存到 `~/.pi/agent/auth.json`，0o600 权限，支持多 provider。

**建议**：
- `/login` 成功后保存到 `~/.yushan/auth.json`
- 启动时从 auth.json 加载已有凭证
- 格式：`{ "deepseek": { "api_base": "...", "api_key": "..." } }`

### 差距 3：Provider-specific compat（中优先级）

**现状**：`ProviderCompat::standard()` 用于所有 provider。`deepseek()` 和 `minimax()` 的 compat flag 是死代码。

**建议**：在 `known_providers()` 中添加 `compat` 字段，factory 根据 provider 选择对应的 compat。

### 差距 4：多 provider 同时登录（低优先级 MVP）

**现状**：单一 api_base/api_key，切换 provider 需要重新 /login。

**Pi 的做法**：每个 provider 独立存储凭证，切换模型时自动找到对应 provider 的凭证。

**建议**：MVP 不需要。当前单 provider 切换（/login 重新登录）可接受。

### 差距 5：Fuzzy 搜索（低优先级）

**现状**：`inquire::Select` 支持输入过滤，但不是 fuzzy。

**建议**：`inquire::Select` 的默认过滤已经够用。fuzzy 搜索是锦上添花，后续可换 `inquire_with_fuzzy` 或其他库。

## 建议的 MVP 改进集

### 必须做

1. **模型列表从 API 动态拉取**
   - 新增 `fetch_models(api_base, api_key) -> Vec<String>` 函数
   - 调用 `GET {api_base}/models` 端点
   - `/model` 优先使用 API 返回的列表
   - 失败时 fallback 到 `known_models()`

2. **凭证持久化到 `~/.yushan/auth.json`**
   - `/login` 成功后写入文件
   - 启动时 `Config::from_env()` 也检查 auth.json
   - `/logout` 从 auth.json 删除对应 provider
   - 文件权限 0o600

3. **Provider-aware ModelFactory**
   - `known_providers()` 增加 `compat` 字段
   - factory 闭包根据 provider 选择 compat flag

### 可以做

4. **`/model` 选择器显示 provider 标签**
   - 从 API 拉取的模型列表中标注 provider 来源
   - 当前模型高亮标记

### 后续迭代

5. 多 provider 同时登录
6. 默认模型持久化
7. Fuzzy 搜索
8. OAuth 支持

## 未澄清问题

- [ ] 模型列表拉取的 API 端点格式？OpenAI 兼容的 `/v1/models` 返回 `{ "data": [{ "id": "..." }] }`
- [ ] auth.json 是否需要加密？Pi 只做 0o600 权限，不加密。
- [ ] Provider 切换时是否需要保留前一个 provider 的凭证？MVP 不需要。
- [ ] `known_models()` 是否保留作为 fallback？建议保留，API 拉取失败时使用。
