---
name: arch-design
description: 多方案架构设计与选择。使用并行 subagent 生成多个截然不同的候选架构，对比分析后推荐方案并记录决策。Use when 设计新的模块或系统架构、面对多个方案需要做选择、需要从不同角度探索设计空间。
---

# Arch Design — 多方案架构设计与选择

## Quick start

当你说"设计 xxx 的架构"时，启动本 skill。核心流程：

1. 确认需求场景、质量目标、约束
2. **调研同类软件**：WebSearch / 代码阅读了解业界同类方案的设计
3. 3 个 subagent 并行生成不同倾向的候选架构
4. 各维度对比分析 + back-of-envelope 估算
5. 推荐方案 + 记录 ADR

**产出文件**（写入 `docs/arch/<需求名>/`）：
- `design.md` — 最终方案（候选方案作为节内嵌）
- `adr-<slug>.md` — 决策记录

## Workflow

### Step 1: 需求确认（轻量）

澄清设计目标，确认关键输入。可引用 `arch-explore` 产出的 `context.md`。

检查清单：
- [ ] 关键场景（成功路径）
- [ ] 异常/边界场景
- [ ] 质量目标（性能、可用性、延迟目标）
- [ ] 约束（技术栈、资源、时间）
- [ ] 非目标（明确不做什么）

### Step 2: 调研同类软件

在动手设计方案前，先了解业界同类软件是如何解决问题的。

- 搜索同类开源项目：WebSearch（关键词：同类场景 + architecture / design / internals）
- 阅读关键设计文档或源码分析
- 提取可借鉴的模式和已知教训
- 记录不适用之处（场景差异、技术栈差异）

调研结果作为共享上下文，输入给 Step 3 的所有 Agent。

### Step 3: 多方案生成（3 subagent 并行）

启动 3 个 subagent，每个代表一种设计哲学，基于 Step 2 的调研结果生成方案：

| Agent | 倾向 | 适用场景 |
|-------|------|----------|
| Agent 1: 最小复杂度 | 最简够用方案，最少模块、最少抽象 | 快速验证、团队小、需求稳定 |
| Agent 2: 可扩展优先 | 灵活性优先，接口抽象、插件机制 | 需求不确定、长期演进 |
| Agent 3: 性能/资源优先 | 极致性能、最小开销、零拷贝等 | 核心路径、资源受限 |

每个 Agent 输出：
- 模块划分与职责
- 关键数据流
- 核心接口（或标记"接口设计详见 design-an-interface"）
- 受 Step 2 中哪些同类软件启发

### Step 4: 对比分析

建立对比矩阵，维度包括：

| 维度 | 说明 |
|------|------|
| 实现复杂度 | 代码量、概念数量、上手成本 |
| 性能特征 | 吞吐量、延迟分布、资源消耗（back-of-envelope） |
| 演进灵活性 | 增减功能、替换组件、多版本 |
| 运维负担 | 部署、监控、排障、配置管理 |
| 风险 | 技术不确定性、依赖风险 |

必要时做 back-of-envelope 估算（QPS 上限、内存上限、延迟 breakdown）。

### Step 5: 推荐与记录

- 推荐一个方案（或从多个中融合）
- 明确记录未选方案及其被否理由
- 写入 `design.md` + `adr-<slug>.md`

## 协作技能

本 skill 过程中可调用：
- [`design-an-interface`](../design-an-interface/SKILL.md) — 细化模块接口设计
- [`ubiquitous-language`](../ubiquitous-language/SKILL.md) — 提取和形式化新术语
- [`fireworks-tech-graph-main`](../fireworks-tech-graph-main/SKILL.md) — 生成架构图

## 参考

详细流程、模板、反模式见 [REFERENCE.md](REFERENCE.md)。
