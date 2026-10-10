# 0032 — 退出时打印 `session: <id>`（三模式一致）

## Status

accepted（2026-10-10）

## 背景

- **需求（原话）**：「每次 coding-agent 结束，需要打印出 session id，方便后续 resume」。
- **现状**：`-p` / `--json` 收尾已把 `session: <id>` 打到 **stderr**（ADR-0026）；TUI 侧没有 ——
  会话号只在 `/status` 里看得到（且显示的是完整路径），**退出即丢**，用户无从 resume。
- **约束**（决定了「在哪里取号、在哪里打印」）：
  - TUI 起屏后 stdout/stderr 都会被冲掉，**只有退出之后**写终端才安全（`docs/architecture.md` §13）；
    TUI 的 stdout 是 `restore_terminal` 回放的 transcript，人读的诊断历来归 stderr。
  - 默认新会话**惰性落盘**（首次追加才写文件）：没跑过回合的会话**没有文件**。
  - **UI 退出 ≠ app 线程收摊**：回合可能正卡在模型调用里，`run_interactive` 只能宽限
    `APP_THREAD_GRACE`（1s）后连 runtime 一起丢。

## 决策

1. **三模式统一**：退出时都打 `session: <id>`（id = 会话文件 **stem**），去向 **stderr**；
   文案由 `main::session_exit_line` 唯一给出。TUI 在退出回放之后打（行落在主屏上），
   `-p` / `--json` 在收尾处打（`persist_single_turn`）。
2. **只对已落盘的会话打**：`is_file()` 为假 ⇒ 不打。惰性新会话若一个回合都没跑过就没有文件，
   打出的 id 连 `--resume` 都找不到（`sessions::resolve` 报「找不到会话」）—— 那是误导而非提示。
3. **会话号经 `app_loop::SessionPathHandle` 取**：启动态由 `run_interactive` 从 `Wiring` 播种，
   `/new` 之后由 app 循环刷新；读取发生在 `rt.shutdown_timeout` **之后**，正在收尾的 `/new`
   因此有机会把新路径写进来。句柄与 `ChannelStatsHandle` 同形（跨生命周期、锁中毒即降级为「无」）。
4. **不动 `ys-tui-coding`**：会话语义与 resume 文案归 app 层；TUI crate 不认识
   `Wiring` / `--resume`，退出提示不该把这份知识拽进去。

## Considered Options

- **只在 TUI 打、且文案带 resume 命令**（`Resume with --resume=<id>`）：与 `-p` / `--json` 的
  既有行**分叉**，同一件事两套措辞；id 本身已可直接粘进 `--resume=`。**不采纳**。
- **写进退出回放**（stdout，当 transcript 的最后一行）：stdout 被重定向（`> log.txt`）时用户就
  看不到了；且这条知识要下沉进 TUI crate。**不采纳**。
- **从 app 线程的 JoinHandle 取路径**（等它收摊）：排除在**长回合中途退出**这条最常见路径上
  取不到号的情形，正是本需求的主要场景。**不采纳**。
- **打完整路径**：`/status` 已给路径；退出行要的是能直接喂 `--resume=` 的 id。**不采纳**。

## Consequences

- 任何一次运行退出后，终端上都有可复制的会话号；空会话不打（不撒谎）。
- app 循环多一个 `SessionPathHandle` 参数，`Request::NewSession` 处多一次 `record`。
- **回归测试**：`main::tests::session_exit_line_requires_persisted_file`、
  `app_loop::tests::session_path_handle_follows_new_session`（两者都做过变异验证：去掉
  `is_file` 守卫 / 去掉 `/new` 处的 `record` ⇒ 变红）。
- 遗留（记账）：panic / 硬退出不走这条路径（panic hook 只负责恢复终端，不认识会话）；
  `-p` / `--json` 的**首回合失败**仍无 meta、无文件、无提示（沿 ADR-0026 的已知缺口）。

## 参考

- 会话落盘与恢复：[`0026-session-default-new-and-resume.md`](0026-session-default-new-and-resume.md)、
  [`docs/design-final/session-default-new.md`](../design-final/session-default-new.md)
- 输出与诊断三分：[`docs/architecture.md`](../architecture.md) §13
- TUI 微决策表：[`docs/design-final/coding-agent-tui.md`](../design-final/coding-agent-tui.md) §8.2 M18
