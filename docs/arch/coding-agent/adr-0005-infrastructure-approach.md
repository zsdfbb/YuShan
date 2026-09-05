# ADR-0005: 基础设施补齐方案选择

## 状态

提议

## 上下文

v0 Runtime 已完成（8 crate, 43 tests），需要补齐 9 项基础设施（含 JsonlSession 持久化）为 Coding Agent 层做准备：
1. ModelRequest 加 system prompt
2. ToolContext 加 cwd/workspace_root
3. ToolError 加 PermissionDenied/Timeout
4. ApprovalHandler 审批机制
5. #[non_exhaustive] 补齐
6. RunLimits 加 tool_timeout + max_tool_output_bytes
7. ModelResponse 加 stop_reason
8. ModelRequest 加 max_tokens/temperature

三个候选方案：
- **A: 最小复杂度** — 最少新概念，直接加字段
- **B: 可扩展优先** — trait + enum 抽象，预留 15 个扩展点
- **C: 性能优先** — 零拷贝 lifetime，sync approval + inline

## 决策

**选择 A 融合 C 的 ToolRegistry 缓存**。

### 关键决策

| # | 决策 | 选项 | 选择 | 理由 |
|---|------|------|------|------|
| 1 | ModelRequest 所有权 | owned vs borrowed `&'a` | **owned** | 避免 lifetime 传播到 Model trait；每轮 ~1KB clone 在 100ms+ 模型 RTT 面前可忽略 |
| 2 | ToolContext 路径类型 | PathBuf vs `&'a Path` | **PathBuf** | 同上；Tool 调用频率低，48B clone 开销可忽略 |
| 3 | ApprovalHandler 异步性 | sync vs async | **async** | TUI 场景需要 async 用户交互；sync + block_on 桥接更复杂 |
| 4 | ToolRegistry::specs() | 每轮重建 vs 构建缓存 | **构建缓存** | C 方案中唯一无风险有实际收益的改进 |
| 5 | System prompt 类型 | Option<String> vs SystemPrompt enum | **Option<String>** | v0 不需要动态模板；enum 的 Dynamic 变体是 v1 需求 |
| 6 | ToolContext 结构 | 直接字段 vs ExecutionEnvironment | **直接字段** | v0 不需要沙箱抽象；ExecutionEnvironment 是 v1 需求 |
| 7 | 工具输出截断位置 | Tool 内部 vs Loop 编排 | **Tool 内部** | 对齐 Pi：每个工具负责自身输出截断（Read=head，Bash=tail），RunLimits 提供默认阈值 |

### 被否决的方案

**B（可扩展优先）**：`SystemPrompt` enum、`ExecutionEnvironment` struct、`PermissionKind` enum 在 v0 用不上，引入 6 个新类型 + 2 个 trait 对 ~2k 行项目过度设计。这些抽象可以在需要时自然引入。

**C（性能优先）**：`ModelRequest<'a>` 改变 `Model::complete` 签名，传播到所有 Model 实现。Rust async + lifetime 交互在 async_trait 下容易产生 `'static` 约束冲突。每轮 500B-2KB 分配 vs 100ms+ 模型 RTT，性能差异 < 0.001%，不构成风险承受的理由。

## 后果

### 正面
- 实现复杂度可控：~250 行新增，~15 文件改动
- 无 lifetime 传播风险
- 所有现有 43 个测试通过（新字段用 Default 值）
- 为 v1 演进留有空间（#[non_exhaustive] 保护）
- Pi 工具接口完全对齐（含 fuzzy matching、legacy format、diff output）

### 负面
- 每轮 ModelRequest 构造有 ~1KB 堆分配（可接受）
- SystemPrompt.Dynamic 等抽象需要在 v1 时引入
- PermissionKind 等权限分类需要在 v1 时引入

### 风险
- `tokio::time::timeout` 需要 tokio 从 dev-dep 升级为生产 dep（影响二进制大小）
- ApprovalHandler 的 async trait 需要 async-trait 依赖从 agent-tool 的 dev-dep 升级为生产 dep
- ToolSpec #[non_exhaustive] 阻断外部测试构造——通过新增 `ToolSpec::new()` 构造器解决
- ToolContext::new() 签名变更影响 2 个调用点
- RuntimeContext::new() 签名变更影响 10 个调用点（1 production + 9 tests）
- OpenAI 适配器需要 v0 内部格式 → OpenAI API 格式的翻译层

### v0 安全原则变更

v0 design.md 原则"本项目不定义沙箱、权限、审批"需更新为：
> "Runtime 内置可选的 ApprovalHandler（默认 AutoApprove），上层项目可替换为真正的审批逻辑。v0 不定义沙箱和权限抽象。"

### 截断策略决策

**工具内截断**（对齐 Pi）：
- Read: head 截断，2000行 / 50KB
- Bash: tail 截断，2000行 / 50KB
- BasicLoop 不做二次截断
- RunLimits 提供 `bash_timeout: Option<u64>`（默认 None，对齐 Pi 无默认超时）
