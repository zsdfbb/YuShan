# tools-align-pi — 架构上下文

> 日期：2026-09-15
> 性质：arch-explore 产出（现状 + 约束 + 范围）。**不做方案设计** —— 方案交 `arch-design`。
> 目标：给 coding agent 补**五个工具**，分**两个切片**：
> **切片一（本批实现）** `grep` / `find` / `ls` —— 离线、走本地子进程；
> **切片二（随后实现，本轮一并设计）** `websearch` / `webfetch` —— 走网络。

## 概述

给 coding agent 补齐**工具面**，分两切片推进：

- **切片一 · 本地搜索**（grep / find / ls）：底座为**外挂环境已有的 `rg`/`fd`**，
  缺失时降级 POSIX `grep`/`find`，**绝不下载**。
- **切片二 · 网络**（websearch / webfetch）：接**可配置的搜索 API**；webfetch 抓网页转 Markdown。
  落点为**新 crate `adapters/tools-web`**。

两切片**共享**：共享截断模块（切片一抽出）、`ToolSpec` 驱动的系统提示词清单、`main.rs` 装配点。
并把系统提示词的工具清单改为**从 registry 驱动**（消除硬编码漂移）。

## 现有架构

### 模块边界

```
ys-core  ──►  ys-tool  ──►  adapters/tools-basic  ──►  apps/coding-agent
（ToolResult） （Tool trait /          （BashTool / ReadTool /        （main.rs 注册）
              Registry / Context）     WriteTool / EditTool）
```

- **`adapters/tools-basic`**：本需求的主战场。当前导出 4 个工具（`lib.rs`）。
- **消费方**：`apps/coding-agent/src/main.rs:249-252`（`AgentBuilder::tool(...)` × 4）。
- **`ys-tool`**：`Tool` trait + `ToolRegistry` + `ToolContext` + `ToolSpec` + 审批。
- **`ys-core`**：`ToolResult { content: String, is_error: bool }`（**扁平字符串**，无图像承载）。

### 核心抽象

```rust
// ys-tool/src/lib.rs —— 注意用 #[async_trait]
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;                                   // 构建时调用一次、缓存
    async fn call(&self, input: Value, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError>;
}

// ys-tool/src/spec.rs
pub struct ToolSpec { pub name: String, pub description: String, pub parameters: Value }

// ys-tool/src/context.rs —— 注意：cwd / workspace_root 现有工具基本不用
pub struct ToolContext<'a> {
    pub boundary: Option<&'a dyn BoundarySource>,  // 工具轮询 is_aborted() 感知中止
    pub cwd: PathBuf,
    pub workspace_root: PathBuf,
}

// ys-tool/src/registry.rs
pub struct ToolRegistry { tools: HashMap<String, Box<dyn Tool>>, specs: Vec<ToolSpec> }
// build() 查重名；specs() 构建时缓存；names() 零分配借用
```

**`ToolError` 四态**（`Execution / InvalidInput / PermissionDenied / Timeout`）→ 在 `BasicLoop`
里被映射成 `ToolResult { is_error: true }` **回喂模型**（ADR-0001 双通道）。registry 未命中亦然。

### 关键数据流

```text
AgentBuilder::tool(t)  ──build()──►  ToolRegistry（specs 缓存）
                                          │
BasicLoop Step 5: registry.specs().to_vec() ──► ModelRequest.tools  ──► 模型
BasicLoop Step 9: registry.get(name) ──► tool.call(args, ToolContext{boundary,cwd,workspace_root})
                                          │
                            tokio::time::timeout(limits.bash_timeout.unwrap_or(300), ...)
```

### 路径解析现状（**不一致，需定口径**）

| 工具 | 如何解析路径 |
|---|---|
| `read` / `write` / `edit` | `self.workspace.join(path)`（构造时注入的 workspace）；`_ctx` **未用** |
| `bash` | `Command::new("sh").arg("-c").current_dir(&self.workspace)`；`process_group(0)`（unix） |

> `ToolContext.cwd` / `workspace_root` **传了但当前无人用**。新工具应沿用 `self.workspace`
> （与 read/write 一致），还是改用 ctx —— 待定（见未澄清问题）。

### 截断现状（**重复**）

| 位置 | 常量 | 逻辑 |
|---|---|---|
| `bash.rs:9-10, 26-61` | `MAX_LINES=2000` / `MAX_BYTES=50*1024` | `tail_truncate()`：保尾，前缀 `[Truncated N lines.]` |
| `read.rs:6-7, 66-97` | 同上（**重复定义**） | 内联 head 截断 + `[Showing lines a-b of n. Use offset=…]` |

**无共享模块。** 而 `grep`/`find`/`ls` 三个新工具**都要**「head 截断 + `[N limit reached]` 提示」。

### 系统提示词现状（**硬编码，会漂移**）

```rust
// apps/coding-agent/src/prompt.rs
let tools = ["read", "write", "edit", "bash"];   // ← 硬编码
fn tool_snippet(name) / tool_guidelines(tools)   // ← 按名字 match
// 调用点：main.rs:537 prompt::build_system_prompt(&cwd) —— 拿不到 registry
```

而 `ToolSpec.description` **也**会经 `ModelRequest.tools` 发给模型 —— 两处描述，**且硬编码那处会漏**。

### 外部依赖

| 依赖 | 用途 | 现状 |
|---|---|---|
| `ys-core` / `ys-tool` | 接口 | 已有 |
| `serde_json` / `async-trait` | 序列化 / trait | 已有 |
| `tokio`（rt / time / io-util / fs / **process** / signal / macros） | 执行 | **已有 `process`** |
| `unicode-normalization` | edit 模糊匹配 | 已有 |
| `regex` / `ignore` / `globset` / `walkdir` | —— | **无**（本需求**不引入**，见范围外） |

## 约束

- **技术**
  - 平台 **仅 Linux / macOS**；**不考虑 Windows**（无 `powershell`、无 win 路径分支）。
  - `ys-tool` 的 `Tool` 用 `#[async_trait]`；新工具照办。
  - `ToolResult` 是**扁平 `String`**，无图像/结构化承载 → read 图像支持**不在本范围**。
  - **不得引入依赖环**：`tools-basic` 是 adapter 层，可依赖 `ys-tool`/`ys-core`，不得反向。
  - **零网络**：**不下载任何二进制**（明确否决 Pi 的 `ensureTool`）。
- **性能**
  - 工具输出必须**有界**：沿用 2000 行 / 50KB 约定，否则大 repo 搜索会撑爆上下文。
  - 搜索必须有 **limit**（grep 默认 100 命中；find 默认 1000；ls 默认 500）。
  - **探测只做一次**（启动期），不得每次调用都探 `--version`。
- **演进**
  - 系统提示词的工具清单**不得再硬编码**（漂移源）。
  - 变更须对应 `docs/design.md` §12 测试矩阵条目。
- **组织**
  - 小团队；关键逻辑**必须做变异测试**（项目约定）；测试隔离用 `test_env::env_lock()`。

## 需求范围

### 范围内

1. 新增三工具：**`grep`**（内容搜索）、**`find`**（按 glob 找文件）、**`ls`**（列目录）。
2. **底座**：优先环境 `rg` / `fd`（含 Debian 别名 `fdfind`）→ 降级 POSIX `grep` / `find`。
3. **不下载**；启动期**探测一次并缓存**；缺 `rg`/`fd` 时启动期给出**一次 stderr 提示**。
4. **降级路径**带硬编码排除目录（`.git` / `target` / `node_modules` / `dist` / `build` / `.venv`）。
5. **抽共享 `truncate` 模块**（head / tail / line + notices），`bash` / `read` 改用，新三工具复用。
   > **已修正（见 `design.md` §1.1）**：落点从 `adapters/tools-basic/src/truncate.rs` 改为
   > **`crates/ys-tool/src/truncate.rs`** —— 切片二的 `tools-web` 也要用它，放 `tools-basic`
   > 会让 adapter 依赖 adapter 并把 tokio 拖进网络 crate。
6. **提示词**：工具清单改由 **registry 驱动**（片段 / guideline 仍留 `prompt.rs` 产品层）。
7. 参数面**照搬 Pi**：grep `pattern/path/glob/ignoreCase/literal/context/limit`；
   find `pattern/path/limit`；ls `path/limit`。

### 切片二：网络工具（范围内，随后实现）

8. **`websearch`**：接一个**可配置的搜索 API**（endpoint + key 由 config / env 提供，默认一家）；
   返回 `title / url / snippet`，各 provider 结果形状不同 → 需**映射到统一 schema**。
9. **`webfetch`**：抓 URL → HTML → **Markdown**（引进一个 HTML→Markdown crate）→ 复用共享截断 → 回喂。
10. **落点**：**新 crate `adapters/tools-web`**（`reqwest` + 转换器）。`adapters/tools-basic` **保持无网络**。
11. **失败模式**（ADR-0001 双通道）：网络错误 / 超时 / HTTP 非 2xx / DNS 失败 → 作为
    **错误结果回喂模型**（可自愈），**不是**基础设施失败。
12. **中止**：网络请求应能响应 `boundary.is_aborted()` 与兜底超时（同 `BashTool`）。
13. **安全**：YuShan「不做安全」，但 webfetch **至少校验 URL scheme（仅 `http`/`https`）**；
    SSRF / 内网防护**不在本轮**。

### 范围外（明确不做）

- `powershell`（Windows 专属）。
- `read` 的图像支持（要动 `ToolResult` 的图像承载，独立一刀）。
- **纯 Rust 重写** grep/find（曾考虑 `ignore` + `regex` + `globset`，**已否决** —— 改外挂）；
  因此**不引入** `regex`/`ignore`/`globset`/`walkdir`。
- 运行期热插拔 / hook 系统（见 ADR-0014）。
- 真上下文压缩（`/compact` 占位问题，独立缺口）。

### 关键场景

- **场景 1**：在 Rust 仓库里 grep 一个符号 → 只出源码命中，**不搜 `target/`**（有 `rg` 时免费，
  降级时靠排除目录兜底）。
- **场景 2**：`find '**/*.spec.ts'` 定位测试文件 → 相对路径列表，超 limit 时给提示。
- **场景 3**：`ls src/` → 排序后的条目，目录带 `/`。
- **场景 4**：机器**没装 `rg`/`fd`** → 降级到 `grep -r` / `find`，**仍能出结果**，启动期提示装 ripgrep。

### 异常 / 边界场景

- 非 git 仓库 / `rg` 缺失 / `fd` 缺失（别名 `fdfind` 也要探）。
- 被调二进制返回非零（`rg` 无命中返回 1，**不是错误**）。
- 单行超长（grep 命中行需截断，Pi 取 500 字符）。
- 输出爆炸（命中/条目超 limit → 截断 + `[N limit reached. Use limit=2N for more]`）。
- 路径不存在 / 不是目录。
- 用户 **Ctrl+C 中止**（长搜索）：工具须轮询 `ctx.boundary.is_aborted()`（同 `BashTool`）。

## 未澄清问题

- [ ] **探测结果放哪**：`tools-basic` 内的 `OnceLock`？还是 app 层探测后注入 `ProviderRegistry` 式的注册表？
- [ ] **谁触发探测、何时**：`main.rs` 启动期？还是工具首次调用时惰性？
- [ ] 降级的**排除目录**：硬编码常量，还是可配（env / settings）？
- [ ] 降级路径下 `grep` 的 `context`（前后文行）怎么实现（`grep -C`）？`glob` 参数（`--include`）？
- [ ] 新工具**是否挂 timeout**？（`bash` 有；`read`/`write` 无。）搜索若挂死谁来收？
- [ ] `ls` 是否要文件大小 / 权限？（Pi 只给名字 + `/`。）
- [ ] 路径口径：新工具用 `self.workspace`（同 read/write）还是 `ctx.cwd`/`workspace_root`？
- [ ] `-p` / `--json` 模式下三个工具**是否同样可用**（应然，但要确认无模式分支）。
- [ ] 提示词从 registry 驱动后，`build_system_prompt` 的签名怎么改（接 registry 还是接 `&[ToolSpec]`）？

**切片二（网络工具）**：

- [ ] **默认的那"一家"搜索 API 是谁**？（选了"可配置、默认一家"，但默认值未指定。）
- [ ] 搜索结果的**统一 schema** 形状（`title / url / snippet` + ?）？各 provider 怎么映射？
- [ ] 搜索 API 的 **endpoint + key 存哪**：`Config`？env？复用 `auth.json` 式持久化？
- [ ] **无 key 时 websearch 怎么表现**：注册但不工作 / 不注册 / 返回明确错误？
- [ ] webfetch 的**下载字节上限 / 超时 / 重定向**策略？
- [ ] **非 HTML**（PDF / JSON / 纯文本）怎么处理？
- [ ] webfetch 是否**缓存**同 URL？
- [ ] 选哪个 **HTML→Markdown crate**？（`html2text` / `readability` / 其他）
- [ ] `tools-web` 的启用是否走 **Cargo feature**？（本项目「静态组合优先：crate + feature」）
- [ ] 网络工具在 `-p` / `--json` 下的行为（应一致）。

## 后续建议

- 用 **`arch-design`** 出方案（探测机制 / 降级语义 / truncate 模块形状 / 提示词接线）。
- 可用 **`prototype`** 验证 `rg --json` 解析与降级输出归一化。
- 参考实现（**只读**）：`tmp/pi/packages/coding-agent/src/core/tools/{grep,find,ls,truncate}.ts`；
  `tools-manager.ts`（其 `ensureTool` 自动下载**是我们明确否决的部分**）。
