# 如何贡献

> 本项目是单人主导的开源项目（`v0.0.1`，接口还会动）。下面是实际的规矩，
> 不是模板话——每一条背后都能追溯到 [`AGENTS.md`](AGENTS.md) 或 `docs/` 里的具体文档。

三条总纲，后文都在展开它们：

1. **先归类，再动手** —— 想法落在 Event / Tool / Follow-up / Hook 哪个桶，决定了它该不该做。
2. **用本项目自己开发** —— 仓库自带 5 个开发 skill，clone 完装一次即可；它们不是装饰。
3. **文档是事实来源** —— 代码改动必须同步 `docs/`，否则下次谁都不知道现在是什么状态。

## 目录

- [开始之前：确认边界](#开始之前确认边界)
- [用本项目自己开发](#用本项目自己开发)
- [环境](#环境)
- [动手之前：先归类](#动手之前先归类)
- [硬性约束](#硬性约束)
- [代码约定](#代码约定)
- [测试](#测试)
- [提交与 PR](#提交与-pr)
- [改动的文档要求](#改动的文档要求)
- [问题排查与方向](#问题排查与方向)
- [附录：加一个工具](#附录加一个工具)

## 开始之前：确认边界

**先确认你要动的东西归谁管。** 本项目的定位是「只做骨架」：

| 归这里管 | 不归这里管 |
|---|---|
| 模型调用、消息状态、工具调用循环、事件流 | 沙箱、隔离、权限、审批、多租户安全 |
| Agent 运行时本身 | 业务级工作流 |

安全相关的需求请去 [seabox](https://github.com/zsdfbb/seabox)。
在这里提「加个权限拦截」会被拒绝——不是不欢迎，是定位冲突（见 [`docs/CONTEXT.md`](docs/CONTEXT.md)）。

## 用本项目自己开发

仓库自带 5 个**开发用 skill**（外加一个 `pdf-analysis` 示例）。它们不是示例代码，
是这个项目日常就是这样迭代的——设计、实现、评审、归档都由这些流程串起来。

### 装一次

```console
$ ./scripts/cargo-build.sh build --release   # 先有二进制
$ ./scripts/install-skills.sh --list         # 看有哪些
$ ./scripts/install-skills.sh                # 默认软链到 ~/.yushan/skills
$ ./target/release/ys-coding-agent           # 重启后生效：skill 目录只在启动期扫一次
```

- 默认是**软链**，`git pull` 后立刻生效，不用重跑；不支持软链的环境用 `--copy`。
- 脚本**不覆盖**目标目录已有的非本仓库同名条目（避免顶掉你自己装的 skill）。
- 扫描根恒为 `<cwd>/.yushan/skills` 与 `~/.yushan/skills`，**顺序即优先级**（项目级胜出）。
  没有环境变量可配——这是 ADR-0019 的 D2′。

### 哪个 skill 配哪个任务

| 你要做的事 | skill | 落在哪 |
|---|---|---|
| 不熟一块代码、需求还糊 | `arch-explore` | `docs/arch/<需求>/context.md` |
| 有几个方案要比较 | `arch-design` | `docs/arch/<需求>/design.md` + `adr-<slug>.md` |
| review 一个设计或已有代码 | `arch-validate` | `docs/arch/<需求>/review.md` |
| 有个难查的 bug / 性能回退 | `diagnose` | 修复 + 回归测试 + 回写 `docs/design-final/` |
| 端到端做一个功能（设计 → 实现 → 归档） | `sequential-workflow` | design-plan / exec-plan / report + 回写 `design-final/` |
| 读论文、合同、扫描件 | `pdf-analysis` | 提取结果（分页落盘，`read` 续读） |

命令名 = 目录名，在 TUI 里直接 `/arch-explore`、`/sequential-workflow` 触发；
在 `-p` / `--json` 模式下同样可用。

### 常见任务走哪条路

| 场景 | 起手 | 收尾 |
|---|---|---|
| 修一个明确的 bug | `diagnose` | 回归测试 + 回写 `docs/design-final/`，需要时补 `docs/reports/<日期>-<主题>-report.md` |
| 小改动（≤3 文件） | `sequential-workflow` 的 `quick` 方案 | 回写 `docs/design-final/` |
| 完整功能（多文件 / 跨 crate） | `arch-*` → `sequential-workflow` 的 `full` 方案 | plan 归档到 `completed/` + 回写 `design-final/` |
| 纯文档订正 | 直接改 | 无（但别与代码打架） |

`sequential-workflow` 的硬要求值得先知道：**所有代码修改都通过 subagent 派发**，
orchestrator 只负责协调、审阅、决策；实现前先建字节级基线（`scripts/baseline.sh`），
review 靠 `git diff <baseline>` 拿证据，而不是看返回消息。

### ⚠️ skill 正文里的 Claude Code 术语

skill 是从 Claude Code 生态里搬来并改造过的。正文里出现的 **plan mode、
`EnterPlanMode` / `ExitPlanMode`、Explore agent** 指的是 Claude Code 的机制；
**本 runtime 没有 plan mode**（系统提示里明确写了 "no plan mode"，
见 `apps/coding-agent/src/prompt.rs`）。

遇到这类指令时的等价做法：

- 「Phase 1 处于 plan mode，只读」→ 就是**先读后写**：这一阶段只用 `read` / `grep` / `find` / `ls`，
  不改文件、不跑有副作用的命令。约束来自你自己守规矩，不是来自平台。
- 「`ExitPlanMode` 后进入 Phase 2」→ 读完、核完、跟用户确认完，再开始写。
- 「派 Explore agent 搜索」→ 本 runtime 有 `subagent` / `subagent_async`，直接派即可
  （子 agent 的工具表和父 agent 一样，别指望它有额外能力）。

skill 里提到的 `docs/arch/`、`docs/design-plans/`、`docs/exec-plans/`、`docs/reports/`
**都是本仓库真实存在的目录**，产物直接落进去，不需要另建结构。

## 环境

```console
$ git clone https://github.com/zsdfbb/YuShan.git && cd YuShan
$ ./scripts/cargo-build.sh build --release
```

- **Rust 1.88+**（edition 2024，见 `Cargo.toml` 的 `rust-version`）。
- **必须用 `./scripts/cargo-build.sh`，不要直接 `cargo build`**。rustc 的增量编译从不删旧的
  `.rcgu.o`，改 `ys-core` 每次涨 6,247 个文件；堆到 92 万时改底层 crate 的增量编译会从 4 秒变成 163 秒。
  脚本会在条目数超阈值时自动清理（`CARGO_GUARD_LIMIT` 可调）。
- **必须用 `./scripts/par-test.sh` 跑全量测试，不要用 `cargo test --workspace`**。后者串行跑各测试
  二进制（≈34 秒），脚本跨二进制并行（≈7 秒），用例完全相同。直接跑 `cargo test --workspace`
  不算跑过测试。

本仓库**没有 CI**。合入前的门禁就是下面这几条本地命令，请自己跑完再提 PR
（见「[提交与 PR](#提交与-pr)」）。

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
- **工具面恒定**：工具不能按登录态或配置增减。工具一旦消失，模型会把「工具没了」
  误读成「我记错了」。降级一律发生在**调用期**（返回明确错误）
- **取消即消息**：每回合必须新建 `BoundarySource`——`Abort` 的标记永久置位，复用会让上一回合的取消立刻取消之后每个回合
- **信道契约**：有界、容量可配；信道满时实现必须内部缓冲，不得返回 `Err`；
  循环内的自由函数 `emit()` 总是走异步路径；`-p` / `--json` 的消费必须与 turn 并发
- **`capabilities.rs` 绝不 print**：一切输出走 `Outbound::Output` → transcript

新增第三方 crate 之前先问一句「标准库或现有依赖能不能做」：本项目长期保持
**零新依赖**完成特性（例如 skills 的 frontmatter 就是自写子集解析，而不是引入 YAML 库）。
需要新增时，在 PR 描述里写清楚为什么。

## 代码约定

- **文档、注释、讨论、commit message、PR 描述用中文**；代码标识符用英文（专名可夹英文）
- **注释只写当前设计**——不写变更叙事（「以前是…」「修过…」），不拿别的产品当理由。
  设计的 why 进 `docs/adr/`，变更过程进 `git log`
- **测试隔离**（踩过坑）：临时目录名必须含进程内原子序号 + `process::id()`；
  进程级 env 改写必须走 `crate::test_env::env_lock()` + `EnvRestore`（RAII 恢复）
- **流式 / 字节路径注意 UTF-8 边界**：缓冲是字节级，只在完整行上解码
- `tmp/` 已被 `.gitignore` 忽略，别把正式内容放进去

## 测试

改动要能对应上 [`docs/design.md`](docs/design.md) §12 的测试矩阵条目。三个层次：

| 层次 | 位置 | 怎么跑 |
|---|---|---|
| 单元 | 各 crate 的 `#[cfg(test)]` | `./scripts/par-test.sh` |
| 集成 | `apps/coding-agent/tests/*.rs`（子进程、无凭证） | 同上 |
| 端到端 | `tests/e2e*.rs`（`#[ignore]`，要真凭证） | `cargo test -p ys-coding-agent e2e -- --ignored --nocapture` |

端到端与基准需要 `YUSHAN_API_BASE` + `YUSHAN_API_KEY`：

```console
$ cargo test -p ys-coding-agent --test e2e_bench -- --ignored --nocapture   # 轮数 / 墙钟 / ttft·gen·tools 切分
$ cargo test -p ys-coding-agent --test e2e_skills -- --ignored --nocapture  # skill 目录段的真实 token 成本
```

**关键逻辑要做变异测试**：注入 bug 验证测试能抓到。本项目多个真 bug（死锁、UTF-8 损坏、
`/new` 失效、单击冻结视窗）都是「测试全绿时依然存在」，靠变异测试与端到端复现才暴露。
仓库里现成的变异脚本（每个都会跑完给汇总表，并保证还原）：

```console
$ python3 scripts/cancellation-mutation.py   # 取消链路（ADR-0046），12 处注入
$ python3 scripts/wheel-mutation.py          # 滚轮接管（ADR-0050），6 处注入
$ python3 scripts/selection-mutation.py      # TUI 选区
$ python3 scripts/theme-mutation.py          # 配色主题
```

PR 里请写明：跑了哪些测试、多少条通过、做过哪些变异注入。**不要为了让测试变绿去放宽断言**——
那等于把 bug 焊死。

## 提交与 PR

### commit message

`conventional commits` 前缀 + 中文一句话结论；正文写**为什么**和**验证**，不写「改了什么」
（`git diff` 已经说了）。仓库里既有提交就是这个风格：

```
feat(tui): 输入框选区 + 复制落到系统剪贴板 + /keybind

复制「不好用」的根因不在选区逻辑，而在写出去的那一步只有 OSC 52：
VS Code 集成终端的 xterm.js 不实现它，而那正是主要使用场景。

- 剪贴板写入改为双发：本机工具 + OSC 52（保留给 SSH / 容器 / tmux）。

验证：ys-tui-coding 497 passed；17 处变异注入全部被抓到；fmt/clippy 干净。

ADR: docs/adr/0057-input-selection-and-system-clipboard.md
```

要点：

- 常见前缀：`feat` / `fix` / `docs` / `test` / `refactor` / `perf` / `chore`，scope 用模块名
  （`tui` / `subagent` / `skills` / `readme`…）
- 决策类改动**必须**在末行给出 `ADR: <路径>`；没有 ADR 就说明只是实现细节
- 验证段写实测数字（用例数、注入数、耗时），不写「测试通过」四个字

### 提 Issue / PR

- **Bug**：附复现步骤、`--json` 输出或 debug 日志片段（`~/.yushan/logs/debug-*.jsonl`，
  `YUSHAN_LOG_DIR` 可改目录）
- **新功能**：先开 Issue 说清楚「它落在哪个桶」（见[上文归类](#动手之前先归类)），
  比直接提 PR 更容易推进；想要设计先行，直接跑 `arch-explore` 把 `context.md` 附上
- **PR**：小而聚焦，一个 PR 做一件事。描述里包含：

  ```text
  - 为什么：对齐哪条 ADR / 解决什么问题（没有就说明是纯实现细节）
  - 怎么验的：命令 + 实测结果（用例数 / 注入数）
  - 文档同步：改了哪些 docs / ADR / AGENTS.md（对照下面的矩阵，写「无」也要写）
  ```

### 提交前自查

```console
$ ./scripts/cargo-build.sh                    # 能编译
$ ./scripts/par-test.sh                       # 全绿（不是 cargo test --workspace）
$ cargo clippy --all-targets && cargo fmt     # 无告警、格式干净
$ bash scripts/doc-consistency.sh             # 文档对账（改过文档时）
```

## 改动的文档要求

**代码改动必须伴随文档**，因为本项目的文档是事实来源：

| 你改了什么 | 需要同步 |
|---|---|
| 新增 / 修改模块或接口 | [`docs/design.md`](docs/design.md) §12 测试矩阵要有对应条目 |
| 任何设计决策（取舍、否决某方案） | 新增 [`docs/adr/`](docs/adr/) 记录 why |
| 已实现的功能 | [`docs/design-final/`](docs/design-final/) 更新 as-built（**含实现与设计的分歧**） |
| 走了一遍完整设计流程 | `docs/arch/<需求>/` 的 context / design / review 留在原地，plan 归档到 `docs/design-plans/completed/`、`docs/exec-plans/completed/` |
| 做完了一件 TODO | 从 [`docs/TODO.md`](docs/TODO.md) **删掉条目**（不是标「已完成」） |
| 改了目录结构 / 命令 / 环境变量 | 同步 [`AGENTS.md`](AGENTS.md) |
| 改了贡献流程本身 | 同步本文件 + [`README.md`](README.md) 的入口 |

**指针纪律**：任何「X 当前是 Y 状态」的静态断言，必须引用 `code/` 或带标记的
`docs/design-final/`。旧 ADR 和旧设计计划只记 **why**，不能用来佐证 **now**。

## 问题排查与方向

找不到方向时看 [`docs/TODO.md`](docs/TODO.md)——那是跨轮次的工作台账，每条都标了还差什么、
凭什么说它还没做。里面有明确欢迎认领的方向（比如子 agent 多角色、跨会话输入历史），
也有**有意不做**的（PTC、动态插件热插拔），后者别浪费时间。

遇到疑难 bug 不要硬猜，先跑 `diagnose`：它的第一步是**建立反馈回路**（能稳定复现的命令），
没有回路之前的所有推理都不可信。

## 附录：加一个工具

以 `ls` 为例，实际要碰四个地方：

```
adapters/tools-basic/src/ls.rs       实现 Tool，含 spec() 与执行
adapters/tools-basic/src/lib.rs      pub use
apps/coding-agent/src/toolset.rs     build_tools() 里 push，且位置即顺序
apps/coding-agent/src/toolset.rs     EXPECTED_ORDER 加名字（与上一条必须同步）
```

⚠️ **工具面恒定**是硬约束（见「硬性约束」）。另有两处连带影响：

- `EXPECTED_ORDER` 与 `build_tools()` **改一处必须改两处**，`cargo test` 会卡住不一致。
- 工具表的**顺序即契约**：子表 JSON 必须是父表的字节前缀，这样 prompt cache 才命中。
  父表追加段（`bash_bg` + `subagent` + `subagent_async`）的顺序不能重排。
  改之前读 [`docs/design-final/coding-agent-tui.md`](docs/design-final/coding-agent-tui.md) §13.1（装配形态在 :723 起）。

---

不确定某件事该不该做？直接开 Issue 问。**问错比做错便宜。**
