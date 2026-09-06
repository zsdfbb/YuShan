# ADR: /login & /model 命令行为改进 — 选择 Provider 注册表方案

## 状态

提议

## 背景

YuShan 的 `/login` 和 `/model` 命令与 Pi 对照存在 5 个关键差距：

1. 凭证不持久化（重启丢失）
2. 模型列表硬编码（3 个静态模型）
3. ProviderCompat 未生效（统一 standard()）
4. Provider 感知缺失（模型列表不按 provider 过滤）
5. /logout 不彻底（不清除 config）

需要选择一种架构方案来改进这些行为。

## 决策

选择 **方案 B：Provider 注册表**的务实简化版。

### 候选方案

| 方案 | 核心思路 | 新代码量 | 新模块 |
|------|---------|---------|--------|
| A: 最小改动 | 在 Config 上加字段，命令内联逻辑 | ~65 行 | 0 |
| **B: Provider 注册表** | **ProviderRegistry 集中管理 provider 知识** | **~200 行** | **1** |
| C: 分层配置 | AuthLayer + ModelLayer + CompatLayer | ~285 行 | 3 |

### 选择理由

1. **消除 `&'static str` 约束**：方案 A 保留 `Provider(&'static str)`，无法干净地关联动态模型。方案 B 用 `ProviderInfo(String)` 彻底解决。

2. **职责内聚**：Provider 知识、凭证持久化、模型获取、compat 选择在逻辑上回答同一个问题——"我们对这个 provider 了解什么？"。一个 registry 自然捕获这个边界。

3. **不过度设计**：方案 C 的三层拆分对 3 个 provider 增加了不必要的复杂度。方案 B 用一个模块达到同等的清晰度。

4. **MVP 范围匹配**：简化版不引入 `CommandContext` 变更、不支持多 provider 同时登录、不持久化模型缓存。保持最小表面积。

### 未选方案的否决理由

- **方案 A**：保留 `&'static str` 约束，Config 混合配置和运行时缓存，ModelCommand 内联逻辑膨胀
- **方案 C**：3 个新模块对当前规模过度设计，ModelLayer 需要不属于自己的凭证参数，增加复杂度

## 后果

### 正面

- `/login` 后凭证持久化，重启自动恢复
- `/model` 显示 provider 实际可用模型
- DeepSeek R1 推理内容、MiniMax 工具调用格式正确处理
- 添加新 provider 只需在 registry 中加一个 `ProviderInfo`

### 负面

- 新增 `provider.rs` 模块（~200 行）
- `reqwest` 依赖引入 coding-agent 二进制
- fetch_models 的 URL 路径构造需处理各 provider 差异（Groq 含 `/v1`，OpenRouter 非标准响应）

### 风险

- MiniMax 的 `/v1/models` 端点曾有 bug（2025.12），虽已修复但需 fallback 保障
- fetch_models 5s 超时可能在慢网络下体验不佳，后续可调整
