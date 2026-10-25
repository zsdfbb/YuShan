# 如何贡献

> 本项目是单人主导的开源项目（`v0.0.1`，接口还会动）。下面是实际的规矩，
> 不是模板话——每一条背后都能追溯到 [`AGENTS.md`](AGENTS.md) 或 `docs/` 里的具体文档。

## 开始之前

**先确认你要动的东西归谁管。** 本项目的定位是「只做骨架」：

| 归这里管 | 不归这里管 |
|---|---|
| 模型调用、消息状态、工具调用循环、事件流 | 沙箱、隔离、权限、审批、多租户安全 |
| Agent 运行时本身 | 业务级工作流 |

安全相关的需求请去 [seabox](https://github.com/zsdfbb/seabox)。
在这里提「加个权限拦截」会被拒绝——不是不欢迎，是定位冲突（见 [`docs/CONTEXT.md`](docs/CONTEXT.md)）。

## 环境

```console
$ git clone https://github.com/zsdfbb/YuShan.git && cd YuShan
$ ./scripts/cargo-build.sh build --release
```

- **Rust 1.88+**（edition 2024）
- **必须用 `./scripts/cargo-build.sh`，不要直接 `cargo build`**。rustc 的增量编译从不删旧的
  `.rcgu.o`，改 `ys-core` 每次涨 6,247 个文件；堆到 92 万时改底层 crate 的增量编译会从 4 秒变成 163 秒。
  脚本会在条目数超阈值时自动清理。
- **必须用 `./scripts/par-test.sh` 跑全量测试，不要用 `cargo test --workspace`**。后者串行跑各测试
  二进制（≈34 秒），脚本跨二进制并行（≈7 秒），用例完全相同。直接跑 `cargo test --workspace`
  不算跑过测试。

## 动手之前：先归类

拿到一个「想让 agent 做 X」的想法，**先判断它落在哪个桶**，再想实现。
判断依据在 [`docs/extension-points.md`](docs/extension-points.md)：

```text
Event      发生了什么      （只观察，不可改流程）
Hook       下一步怎么处理    （可改流程；本项目有意推迟，见 ADR-0014）
Component  谁来提供能力     （Model / Tool / Session / EventSink）
Loop       按什么规则推进
```

一句话动机：**本项目有意不建通用 hook 系统**。绝大多数「看起来要 hook」的需求，
其实是 **Event** 或者 **Tool**。

判断链的核心是三个问题：

1. 只是想知道某事发生了，还是要**改变**流程？ → 前者是 Event 消费者，到此为止
2. 是模型决定何时做，还是无论如何都必然发生？ → 前者是 Tool
3. 发生在回合之内还是回合之间？ → 回合内改工具自己；回合之间是 Follow-up

## 硬性约束

这些是 [`docs/design.md`](docs/design.md) 的不变量加历次 ADR 的摘要。
**新增代码违反任何一条都算设计偏离，需要先改设计文档。**

- **依赖方向单向**：`ys-core` ← 组件接口 ← loop/runtime ← 应用适配器
  - `ys-core` / `ys-event` / `ys-protocol` **不含 tokio**（直接与间接皆无）
  - `ys-tui-coding` **不依赖 `ys-runtime`**——渲染层不认识 `Agent`，这条靠 crate 边界强制，不靠自觉
  - **UI 线程必须无 runtime 上下文**（它用 `blocking_send`），这条编译器管不住
- **不跨动态库边界传递 Rust trait object、Tokio 类型或跨库所有权对象**
  - 推论：`ModelEventSink` 必须保持同步 `fn`
- **不做安全**：`ToolRegistry` 只按名字查找工具
- **Agent 无状态**：不持有 session / events / model
- **取消即消息**：每回合必须新建 `BoundarySource`——`Abort` 的标记永久置位，复用会让上一回合的取消立刻取消之后每个回合
- **信道契约**：有界、容量可配；信道满时实现必须内部缓冲，不得返回 `Err`；
  循环内的自由函数 `emit()` 总是走异步路径；`-p` / `--json` 的消费必须与 turn 并发
- **`capabilities.rs` 绝不 print**：一切输出走 `Outbound::Output` → transcript

## 代码约定

- **文档、注释、讨论、commit message、PR 描述用中文**；代码标识符用英文（专名可夹英文）
- **注释只写当前设计**——不写变更叙事（「以前是…」「修过…」），不拿别的产品当理由。
  设计的 why 进 `docs/adr/`，变更过程进 `git log`
- **测试隔离**（踩过坑）：临时目录名必须含进程内原子序号 + `process::id()`；
  进程级 env 改写必须走 `crate::test_env::env_lock()` + `EnvRestore`
- **关键逻辑要做变异测试**：注入 bug 验证测试能抓到。本项目多个真 bug（死锁、UTF-8 损坏、`/new` 失效）
  都是「测试全绿时依然存在」，靠变异测试才暴露

## 提交

commit message 用 conventional commits 前缀 + 中文描述，例如：

```
feat(skills): 仓库自带 skill 目录 + pdf-analysis + 安装脚本
fix(tui): 修选区高亮少算 4 行，贴 Input 的几行选不中
docs(readme): 重写 README 并附同类 CLI 的内存实测
```

提交前请确认：

```console
$ ./scripts/cargo-build.sh          # 能编译
$ ./scripts/par-test.sh             # 全绿
$ cargo clippy --all-targets && cargo fmt
```

## 改动的文档要求

**代码改动必须伴随文档**，因为本项目的文档是事实来源：

| 你改了什么 | 需要同步 |
|---|---|
| 新增 / 修改模块或接口 | [`docs/design.md`](docs/design.md) §12 测试矩阵要有对应条目 |
| 任何设计决策（取舍、否决某方案） | 新增 [`docs/adr/`](docs/adr/) 记录 why |
| 已实现的功能 | [`docs/design-final/`](docs/design-final/) 更新 as-built（**含实现与设计的分歧**） |
| 做完了一件 TODO | 从 [`docs/TODO.md`](docs/TODO.md) **删掉条目**（不是标「已完成」） |
| 改了目录结构 / 命令 / 环境变量 | 同步 [`AGENTS.md`](AGENTS.md) |

**指针纪律**：任何「X 当前是 Y 状态」的静态断言，必须引用 `code/` 或带标记的
`docs/design-final/`。旧 ADR 和旧设计计划只记 **why**，不能用来佐证 **now**。

## 想加一个工具？

以 `ls` 为例，实际要碰四个地方：

```
adapters/tools-basic/src/ls.rs    实现 Tool，含 spec() 与执行
adapters/tools-basic/src/lib.rs   pub use
apps/coding-agent/src/toolset.rs  注册进工具表
apps/coding-agent/src/toolset.rs  EXPECTED_ORDER 加名字
```

⚠️ **工具面恒定**是硬约束：工具不能按登录态或配置增减。工具一旦消失，模型会把「工具没了」
误读成「我记错了」。降级一律发生在**调用期**（返回明确错误），而不是从工具表里摘掉。

改动会动到 `EXPECTED_ORDER` 和前缀指纹锚——后者影响缓存优化（子表 JSON 是父表字节前缀），
改之前请读 [`docs/design-final/coding-agent-tui.md`](docs/design-final/coding-agent-tui.md) §19（工具表前缀性质在 :723）。

## 提 Issue / PR

- **Bug**：附复现步骤、`--json` 输出或 debug 日志片段（`~/.yushan/logs/debug-*.jsonl`）
- **新功能**：先开 Issue 说清楚「它落在哪个桶」（见上文归类），比直接提 PR 更容易推进
- **PR**：小而聚焦。一个 PR 做一件事，附上为什么（对齐哪条 ADR / 解决什么问题）

## 找不到方向？

看 [`docs/TODO.md`](docs/TODO.md)——那是跨轮次的工作台账，每条都标了还差什么、凭什么说它还没做。
里面有明确欢迎认领的方向（比如子 agent 多角色、跨会话输入历史），也有**有意不做**的（PTC、动态插件），
后者别浪费时间。

---

不确定某件事该不该做？直接开 Issue 问。**问错比做错便宜。**
