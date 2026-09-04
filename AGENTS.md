# YuShan — 轻量、组件化的 Rust Agent Runtime

用于组装 Agent 的运行时（harness），不是自带全部功能的完整 Agent 产品。
**当前状态：设计阶段，尚无代码。** 实现前先读 `docs/design.md`；文档与代码有冲突时，先改文档达成一致再写代码。

## 目录结构

- `docs/design.md` — 总体设计文档：核心原则、crate 划分、核心 trait、Hook/Event 边界、路线图、测试矩阵
- `tmp/` — 本地草稿与参考资料，已被 `.gitignore` 忽略，勿将正式内容放入
- `tmp/pi/` — 参考项目 pi 的本地克隆（TypeScript 编码 agent），仅作架构对照

## 常用命令

（Cargo workspace 尚未建立，脚手架完成后即为以下标准命令）

- `cargo build` — 构建
- `cargo test` — 测试
- `cargo clippy --all-targets` — lint
- `cargo fmt` — 格式化

## 硬性约束

以下是 `docs/design.md` 的不变量摘要，细节以原文为准。新增代码违反任何一条都算设计偏离，需先改设计文档。

- 依赖方向单向：`agent-core` ← 组件接口（model/tool/session/event）← loop/runtime ← 应用适配器；`agent-core` 不依赖 Tokio、HTTP 客户端、数据库、TUI 或具体模型 SDK
- 最小核心：通用 Runtime 只提供一次 Agent Turn 所需能力；文件系统、Shell、MCP、TUI、记忆、子 Agent 均为可选组件，coding 语义（read/write/edit/bash、项目上下文）只存在于 `apps/coding-agent` 产品层
- 本项目不做安全：沙箱、隔离、权限、审批、多租户由上层项目解决；`ToolRegistry` 只按名称查找工具，不做权限判断
- 四者边界不可混淆：Event 只观察「发生了什么」，Hook 才决定「下一步怎么处理」，Component 提供能力，Loop 决定推进规则
- 静态组合优先：能力通过 crate + Cargo feature 组合；动态插件只做运行时扩展，不承担安全边界，第一版不支持运行期热插拔
- 不跨动态库边界传递 Rust trait object、Tokio 类型或跨库所有权对象；动态插件只走稳定 ABI + 序列化数据

## 工作约定

- 实现顺序遵循 `docs/design.md` §11 路线图：最小闭环 → 可用适配器 → 静态组件生态 → 动态插件 → Coding Agent MVP；不要跳步提前引入后续阶段的东西
- 新模块和接口改动须能对应上 `docs/design.md` §12 测试矩阵中的条目
- 文档、讨论、commit message 用中文；代码标识符与注释用英文

## 参考资料

- 设计全文：`docs/design.md`（Hook 点与优先级 §6，动态插件 ABI §8，crate 结构 §9，Coding Agent 产品层 §10）
- 术语表：`CONTEXT.md`；架构决策记录：`docs/adr/`
- 风格参考：`tmp/pi/AGENTS.md`（行为规则式写法，仅本地可见）
