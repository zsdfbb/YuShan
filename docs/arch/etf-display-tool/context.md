# ETF DisplayTool — 架构上下文

## 概述

为 ETF 分析 Agent 提供 HTML 可视化报告生成能力：Agent 调用 DisplayTool，传入结构化数据（表格、图表、文本），工具生成自包含 HTML 文件到本地磁盘，用户在浏览器中查看。

## 现有架构

### 模块边界

DisplayTool 是 `adapters/tools-finance` 中的一个 Tool 实现，与 MarketDataTool、IndicatorTool 平级。它：
- **输入**：Agent 传入的 `serde_json::Value`（结构化报告数据）
- **输出**：`ToolResult { content: 文件路径, is_error: false }`
- **副作用**：写入 HTML 文件到 `~/.yushan/finance/reports/`
- **不访问** SQLite 数据库（纯渲染工具，数据由 Agent 从其他工具获取后组装）

### 核心抽象

当前 Tool 体系：

```
Tool trait { spec() → ToolSpec, call(Value, ToolContext) → Result<ToolResult, ToolError> }
ToolResult { content: String, is_error: bool }    ← 所有工具都返回纯文本
ContentBlock::ToolResult { content: String, is_error: bool }  ← 消息流中也是纯文本
```

**关键约束**：`ToolResult.content` 是扁平字符串。DisplayTool 不改变这个约束——它的返回值就是文件路径字符串。HTML 文件是磁盘上的独立产物，不进入消息流。

### 关键数据流

```
用户: "回测 510300 过去一年的表现"
          │
          ▼
    Agent (LLM)
    ├── 调用 MarketDataTool → 获取日线数据（纯文本）
    ├── 调用 IndicatorTool → 计算技术指标（纯文本）
    ├── 自身推理 → 组装回测结果 + 分析结论
    │
    └── 调用 DisplayTool
         输入: { title, sections: [table, chart, text, ...] }
         输出: ToolResult { content: "/path/to/backtest_20260912.html" }
          │
          ▼
    Agent 告诉用户: "报告已生成: /path/to/backtest_20260912.html"
          │
          ▼
    用户在浏览器打开文件查看
```

**核心设计**：数据流经 LLM 中转。其他工具返回纯文本 → LLM 解读 → LLM 组装结构化 JSON → DisplayTool 渲染。LLM 是"智能层"，决定什么数据可视化、怎么可视化。

### 外部依赖

| 依赖 | 用途 | 备注 |
|------|------|------|
| ECharts 5 (CDN) | 图表渲染（折线、柱状、饼图） | HTML 内 `<script src="cdn.jsdelivr.net">` |
| agent-tool | Tool trait | 路径依赖 `../../crates/agent-tool` |
| agent-core | ToolResult 等类型 | 路径依赖 `../../crates/agent-core` |
| serde_json | JSON 解析 | 输入数据结构化 |
| chrono | 文件名时间戳 | `YYYYMMDD_HHMMSS` 命名 |
| dirs | 报告目录定位 | `~/.yushan/finance/reports/` |

## 约束

- **技术**：DisplayTool 只实现 `Tool trait`，不修改核心 crate；HTML 为自包含单文件（内联 CSS/JS + CDN 图表库）
- **性能**：报告生成在毫秒级（字符串拼接 + 文件写入）；图表渲染在浏览器端完成，不受工具侧限制
- **演进**：v1 不支持报告更新/覆盖（每次生成新文件）；不支持离线图表（CDN 不可用时图表区域留空，表格/文字正常显示）
- **组织**：ETF Agent 专用，不作为 YuShan 通用基础设施

## 需求范围

### 范围内

- DisplayTool 实现（adapters/tools-finance/src/display.rs）
- HTML 模板系统（Rust `format!` 拼接，const 模板字符串）
- 三种 Section 类型：text（文本段落）、table（表格）、chart（ECharts 图表）
- 报告输出到 `~/.yushan/finance/reports/`，文件名含时间戳
- 深色主题、中文排版
- HTML 转义防护（escape_html 处理所有 agent 传入文本）

### 范围外（明确不做的）

| 不做 | 原因 |
|------|------|
| 修改 Tool trait / ContentBlock | 破坏核心抽象，ETF Agent 不应改核心 |
| TUI 内渲染图表 | 当前 TUI 是纯文本，改造量大且非必须 |
| 实时 HTTP Server 推送 | 用户选择手动打开文件，不需要 server |
| 报告更新/覆盖 | v1 保持无状态，每次新文件 |
| 嵌入 ECharts 到 HTML（base64） | 增加 ~1MB/文件，CDN 方案够用 |
| 通用 DisplayTool 框架 | ETF Agent 专用，不过度设计 |

### 关键场景

- **场景 1：回测报告** — Agent 运行回测后，生成包含收益曲线图、绩效指标表格、回撤分析图、文字结论的 HTML 报告
- **场景 2：ETF 对比** — 生成包含对比表格（费率、跟踪误差、规模）+ 叠加净值曲线图的 HTML
- **场景 3：估值分析** — 生成包含 PE/PB 历史分位图 + 当前估值文字解读的 HTML

## 未澄清问题

- [ ] 报告目录是否需要配置（当前硬编码 `~/.yushan/finance/reports/`），还是写入当前工作目录？
- [ ] ECharts CDN 不可用时，是否需要 fallback（纯表格/文本的降级页面）？
- [ ] 是否需要在 ToolSpec description 中写明 JSON schema 示例，帮助 LLM 构造正确输入？

## 后续建议

- 建议用 `prototype` 验证：生成一个回测报告 HTML 样例，确认 ECharts 图表效果和排版
- 实现顺序：先做 DisplayTool 骨架（空模板 + 文件写入） → 再加 section 渲染 → 最后加 chart 支持
