# 0067 — 运行读数退出人读出口：`-p` / `--json` 与 TUI 只给结果

首次落地的「每回合一条读数行」（`turn=… rounds=… tpr_x100=…`，ADR-0062 决策 1）
被展示在三个非 debug 出口上：`-p` / `--json` 的 stderr、TUI transcript、状态栏之外的
命令输出。对调用者而言那是噪音 —— `-p` 的调用方要的是**结果**；TUI 用户要的是**对话
与「现在几点 / 跑了多久」**。读数本身仍有价值，但价值在事后分析而不在当场展示。

## Status

accepted（2026-11-02）。取代 ADR-0062 决策 1 的**出口部分**（度量口径与 `run_reading`
记录不变）。as-built 见 `docs/design-final/task-latency-levers.md`「四个出口」与
`docs/design-final/usage-observability.md`「回合级读数记录」。

**后续变更**：§2 表格里的 `Resume with --resume` 一行已随该提示本身下线（见
`docs/architecture.md` §12「默认新会话」）。TTY 门控本身不变。

## 背景

- `summary_line` 的每个字段都是**调优量**（`tpr_x100` / `input_med` / `ttft_ms` …），
  用词面向的是「分析一次运行花了多少」，不是「读完这次回答」。
- `-p` 是面向脚本与外部消费方的模式：stdout 必须是结果、stderr 必须安静。一行
  与结果无关的机器读数混在 stderr 上，等于给每个调用方加一条要过滤的噪音。
  更糟的是**位置**：它写在终局事件之后、`run_result` 错误传播之前，看起来像是错误信息。
- TUI 的读数是插在事件流与终局 `View` 之间的唯一 `Outbound::Output`。它把「对话」
  区域变成了运行日志的第二个容器，而 transcript 的职责是只装对话与命令输出。
- 与此同时，用户真正想要的两个数（**现在几点**、**这次会话跑了多久**）在 `-p` 里
  完全不可见，在 TUI 里只在状态栏末尾，且空闲时因 tick 只服务 Working 动画而不刷新。

## 决策

### 1. 读数**只**落 debug 与 bench

`ev="run_reading"`（含 `line` 与顶层数字字段）与 `bench-summary.json` 原样保留，作为
读数的唯一去处；三模式都不再打印。

**`-p` / `--json`**：`finish_after_reading` 不再接收读数行，**只在消费侧出错时**写一行
`<消费错误>` 到 stderr。正常路径 stderr 完全干净。错误优先级不变（回合错误先于消费
错误），消费异常仍先于回合结果落盘 —— 否则回合失败会把它挡住。

**TUI**：`app_loop` 不再把读数行发进 `Outbound::Output`，transcript 只装对话与命令输出。

### 2. TUI 状态行尾段：本地钟点 + 会话时长

`status_text`（`crates/ys-tui-coding/src/draw.rs`）在剥除顺序的**最末**追加 `HH:MM`：

```
⏸ Ready · deepseek-chat · ↑1.2k ↓345 · 2 轮 · 1m23s · 14:03
```

- **位置在最末即最先被剥**：「现在几点」是背景信息，窄终端下应让位给「在跑什么、
  跑了几轮」。`0s`（会话时长）留在钟点之前。
- **无需新节拍**：事件循环本来就每 `POLL_INTERVAL`（50 ms）无条件重绘一次，故在渲染
  时读钟点即可自行行走；不把 `Working` 的 300 ms tick 扩成全局节拍。
- **时区转换只用一个数**：`libc::localtime_r` 的 `tm_gmtoff`（`crates/ys-tui-coding/src/clock.rs`），
  取不到就退回 UTC。**不引 `time` / `chrono`** —— 本仓只要一个偏移量，`time` 的
  `local-offset` 会连带编译 deranged / num-conv / powerfmt / time-core 整条链。
  `libc` 是目标条件下的新直接依赖（`[target.'cfg(unix)'.dependencies]`），已在依赖树内。
- **可测性**：`App.clock_now: Option<String>` 允许测试注入固定钟点，否则剥段断言会
  依赖「跑测试时机器在哪个时区、几点」。

### 3. `/status` 不动

`duration`（存活）/ `executed`（Σ 回合实际占用）两个口径刻意保留（ADR-0062 A1）：
实测存在「存活 15.10 h / 执行 3.22 h」的形态，混成一个词会被误读。

### 2. 状态行**不该有的东西**也一并退出人读出口

在 ② 之后追加一轮：「收尾提示 + 可发现性提示」也只在 **stdout 是终端**时打
（`stderr_is_human_facing`）。范围：

| 行 | 非 TTY（管道 / 脚本） | 终端 |
|---|---|---|
| `session: <id>`（退出时） | **不打** | 打 |
| `worktree: <name>`（退出时） | **不打** | 打 |
| `Resume with --resume`（启动期可发现性） | **不打** | 打 |
| `worktree create` 的建树告知并入的启动期提示 | **不打** | 打 |
| 纯建树分支（本就没有 TUI / 会话） | 打（它就是「未启动 TUI」说明的一部分） | — |

判定用 **stdout 是不是终端**，不是模式：`-p` 亲手在终端里跑也算人；反之 TUI 虽能起屏，
但能起屏本就意味着 TTY。`stdin` 不参与（`echo q | yushan -p …` 仍是人）。

直接后果：**一次性模式的 stderr 正常路径下完全为空**。集成测试原先大量靠
`session: <id>` 拿会话 id / 断言退出提示，现改为从会话目录与 meta 取证；
TTY 门控本身由 `main.rs` 的 `exit_line_is_silent_when_stderr_is_not_human_facing` 钉住。

## Considered Options

- **保留 `-p` 的读数行，加 `--quiet` 开关**：默认仍污染 stderr，且要求每个调用方
  知道有这个开关。否决 —— 默认应当是安静的。
- **把 `tpr_x100` 等换成「人话」后仍打印**：它依旧与调用方要的结果无关，且人话版
  无法用于跨运行对比（定形行才有价值）。否决。
- **会话退出时打印一次汇总**：`-p` 已经有退出摘要路径（`session: <id>`），再叠一行
  运行读数同样与结果无关。否决。
- **在 TUI 引 `chrono` / `time`**：为一个 `HH:MM` 编译整条时区库。否决，见决策 2。

## Consequences

- **调用方**：`-p` / `--json` 的 stderr 在正常路径上恒为空，脚本不必再过滤读数行；
  「这次跑了多久」从 `-p` 消失 —— 需要它的调用方可直接读 debug JSONL 的 `turn_end`
  / `run_reading`，或自行计时（进程墙钟是它们本来就有的）。
- **会话 id 不再自动出现在管道里**：靠 `session: <id>` 取 id 的脚本得改为扫会话目录
  （文件名即 id）或读自身日志。这是有意取舍：要 id 的是少数脚本，被噪声困住的是全部调用方。
- **调优闭环不变**：ADR-0062 的验收读 `bench-summary.json` 与 debug，不看 stdout。
- **新增一个直接依赖**：`libc`（仅 unix，已在依赖树内，无新下载）。
- **状态行多一段**：窄终端下最先被剥，不影响既有剥除优先级。
- **测试**：`round_clock` 的定形行断言不变（它现在只服务于 debug 与 bench）；
  接线层断言改为「stderr 干净 / transcript 不含读数」；`clock.rs` 新增纯算术用例
  （整点、东偏移、负偏移跨日、午夜回绕）。

## 关联

- [ADR-0062](0062-task-latency-levers.md) — 度量口径与 `run_reading` 记录（本 ADR 只改出口）
- [ADR-0061](0061-usage-buckets-and-attribution.md) — 用量口径与归因
- `docs/design-final/task-latency-levers.md` — 四个出口的 as-built
- `docs/design-final/usage-observability.md` — `run_reading` 的载荷与消费者
- `docs/architecture.md` §13 — 输出与诊断三分
