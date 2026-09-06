# /login & /model 命令行为改进 — 架构质量分析报告

## 分析范围

- **对象**：`docs/arch/commands/login-model-behavior/design.md` 中推荐的方案 B（Provider 注册表务实简化版）
- **维度**：可行性、可维护性、可理解性、性能与可靠性
- **参考**：context.md、adr-provider-registry.md、现有代码（config.rs / builtin.rs / mod.rs / main.rs）

## 综合判断

| 维度 | 判断 | 说明 |
|------|------|------|
| 可行性 | 🟡 黄 | 技术可实现，但设计与代码之间有 2 个未解决的结构矛盾 |
| 可维护性 | 🟡 黄 | 模块边界清晰，但 Config ↔ Registry 职责重叠 |
| 可理解性 | 🟢 绿 | 概念一致，抽象层次匹配，新人可理解 |
| 性能与可靠性 | 🟢 绿 | Fallback 策略合理，超时设置适当 |

---

## 详细分析

### 1. 可行性 🟡

**F1 [高] CommandContext 与 ProviderRegistry 的所有权矛盾**

设计文档自相矛盾：
- design.md 第 57 行：「不引入 `CommandContext` 变更 — registry 作为 Config 的内部字段」
- design.md 第 164 行（文件变更清单）：「`CommandContext` 增加 registry 字段（或通过 Config 间接访问）」
- 计划文件 Step 3：明确写了 `CommandContext` 增加 `pub registry: &'a mut ProviderRegistry`

**问题**：`ProviderRegistry` 是一个有状态的 struct（持有 `auth_store` 和 `model_cache`），它不能同时被 `Config` 持有又被 `CommandContext` 独立引用。当前 `CommandContext` 只借用了 `&mut Agent` 和 `&mut Config`，如果 registry 放在 Config 内部，命令通过 `ctx.config` 访问是自然的；如果 registry 独立存在，则需要在 `tui.rs` 中单独持有并传入。

**建议**：明确选择一种方案：
- **方案 1（推荐）**：Registry 作为 `Config` 的 pub 字段。`CommandContext` 不变。命令通过 `ctx.config.registry.xxx()` 访问。`main.rs` 创建 registry 后写入 config。
- **方案 2**：Registry 独立存在，`CommandContext` 增加 `registry` 字段。`tui.rs` 同时持有 config 和 registry。

**F2 [中] Config 字段与 Registry 内部状态的重复**

当前 `Config` 已有 `api_base`、`api_key`、`model`、`provider` 字段。`ProviderRegistry` 的 `auth_store` 也存储相同的字段（`AuthEntry { api_base, api_key, model }`）。

**问题**：两处存储同一份数据，状态同步是隐患。例如 `/login` 后：
1. `registry.save_auth()` 写入 auth.json ✅
2. `ctx.config.api_base = Some(...)` 更新 Config ✅
3. 如果步骤 2 失败（理论上不会，但逻辑上）→ auth.json 有数据，config 没更新

**建议**：`Config` 的 `api_base`/`api_key`/`model`/`provider` 应视为「当前活跃会话的凭证」，`registry.auth_store` 是「持久化存储」。文档中应明确这个语义区别，并说明 Config 字段是 auth.json 的运行时镜像。

**F3 [低] 当前代码已有部分实现**

`config.rs` 已被修改（git diff 显示 +225 行），包含了 `Provider::compat()`、auth 持久化函数、`fetch_models()` 等。设计方案假设从零开始，但实际上需要先 revert 或整合这些已有改动。

**建议**：实施前先 `git stash` 或评估哪些已有代码可以复用。

### 2. 可维护性 🟡

**M1 [高] ProviderRegistry 职责过重**

`ProviderRegistry` 同时承担 4 个职责：
1. Provider 静态目录（`providers: Vec<ProviderInfo>`）
2. 凭证持久化（`auth_store` + 文件 I/O）
3. 动态模型获取（`fetch_models` + HTTP + 缓存）
4. Compat 选择（`current_compat`）

这不是「职责内聚」，而是「把所有 provider 相关的东西放一起」。如果后续需要单独测试凭证持久化（mock HTTP），或单独测试模型获取（mock 文件系统），都需要拆分 registry。

**建议**：MVP 可以接受，但应在 `provider.rs` 内部用清晰的 `impl` 块分组，并在注释中标注「这些职责后续可能独立」。

**M2 [中] fetch_models 的错误传播不完整**

设计文档只说「API 失败 → fallback 到 known_models()」，但没定义：
- `fetch_models` 返回 `Result<Vec<ApiModel>, String>` → 调用方如何区分「超时」和「401 未授权」？
- 401 应该提示用户重新 `/login`，超时应该静默 fallback
- 当前设计把错误信息丢弃，统一 fallback，丢失了诊断信息

**建议**：定义一个 `FetchModelsResult` 枚举：
```rust
enum FetchModelsResult {
    Success(Vec<ApiModel>),
    AuthError(String),    // 401 → 提示用户
    NetworkError(String), // 超时/DNS → 静默 fallback
}
```

**M3 [中] auth.json 并发安全**

设计提到 Pi 使用 `proper-lockfile` 做文件锁。YuShan 的设计没有提及并发保护。如果用户同时运行两个 YuShan 实例（或终端 + IDE 插件），同时写 auth.json 可能导致数据损坏。

**建议**：MVP 可以接受不加锁（单实例使用场景），但应在文档中注明这个限制。

**M4 [低] reqwest 引入对编译时间的影响**

`reqwest` 是一个重量级依赖（数百个子依赖），会显著增加首次编译时间和二进制大小。

**建议**：可接受，但应在设计文档的「负面」后果中提及。

### 3. 可理解性 🟢

**U1 [好] 概念映射清晰**

`ProviderInfo` → 「一个 provider 是什么」
`AuthEntry` → 「登录凭证」
`ProviderRegistry` → 「所有 provider 的知识库」
这些概念与用户心智模型一致。

**U2 [好] 数据流可追踪**

`/login` → `save_auth` → `fetch_models` → `build_model` → `set_model`，每步有明确的输入输出。

**U3 [中] 「务实简化版」的边界不明确**

设计说「简化版不引入 CommandContext 变更」，但计划文件 Step 3 又要改 CommandContext。读者会困惑：到底改不改？

**建议**：在设计文档中删除「不引入 CommandContext 变更」这句话，或明确改为「CommandContext 增加 registry 字段」。

### 4. 性能与可靠性 🟢

**P1 [好] Fallback 策略合理**

API 成功 → 用 API 列表；失败 → 用静态列表。用户不会因为网络问题而无法使用 `/model`。

**P2 [好] 5s 超时适当**

对于模型列表拉取（通常 <100 模型，响应 <10KB），5s 超时足够宽松。

**P3 [中] 同步文件 I/O 在 async 上下文中**

`save_auth()` 和 `load_auth()` 使用 `std::fs::write` / `std::fs::read_to_string`，这是阻塞调用。在 async 命令处理器中调用会阻塞 tokio runtime 线程。

**建议**：对于小文件（auth.json 通常 <1KB），阻塞时间可忽略。但严格来说应使用 `tokio::fs`。MVP 可以接受，后续优化。

**P4 [低] 无重试机制**

`fetch_models` 失败后直接 fallback，无重试。对于瞬时网络抖动，一次重试可以显著提高成功率。

**建议**：MVP 不需要。可在后续迭代中添加 1 次重试。

---

## 风险排序（影响 × 可能性）

| 排序 | 风险 | 影响 | 可能性 | 建议 |
|------|------|------|--------|------|
| 1 | CommandContext 所有权矛盾导致实施时返工 | 高 | 高 | **立即解决**：在设计文档中明确 registry 的归属 |
| 2 | Config ↔ Registry 数据重复导致状态不一致 | 中 | 中 | 明确语义：Config 是运行时镜像，Registry 是持久化存储 |
| 3 | fetch_models 丢弃错误信息，401 无法诊断 | 中 | 中 | 定义 FetchModelsResult 枚举 |
| 4 | auth.json 并发写入损坏 | 低 | 低 | 文档注明单实例限制 |
| 5 | reqwest 编译时间增加 | 低 | 高 | 可接受，记录在案 |

## 改进建议

### 易修复（实施前可直接改）

1. **删除设计文档中的矛盾语句**：统一为「CommandContext 增加 registry 字段」或「registry 作为 Config 字段」
2. **明确 Config 与 Registry 的数据语义**：在设计文档中增加一节说明
3. **定义 FetchModelsResult 枚举**：替代 `Result<Vec<ApiModel>, String>`

### 需讨论（需与用户确认）

4. **Registry 归属**：放 Config 内部 vs 独立传入 CommandContext？推荐方案 1（Config 内部）
5. **是否需要 tokio::fs**：当前同步 I/O 在小文件场景下可接受，但严格性有差异

### 架构级（影响长期演进）

6. **ProviderRegistry 的职责拆分预留**：当前不拆，但应在模块内用注释标注后续独立方向
7. **auth.json 格式版本化**：当前格式简单无需版本号，但应在设计中预留（如增加 `"version": 1` 字段）
