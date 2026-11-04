# 0069 — TUI 空闲按需重绘与终端挂断看守

空闲期不再以 20 fps 无条件重绘，改为「状态变化才画 + 状态行 1 Hz 秒数刷新」；
unix 下新增独立看守线程，终端挂断即恢复终端并退出进程 —— 消除空闲 CPU 常驻，
以及终端消失后 crossterm `poll` 自旋导致的孤儿进程 100% CPU。

## Status

accepted（2026-11-04）。as-built 见 `docs/design-final/coding-agent-tui.md` §2；
三信道 / 两线程拓扑不变（ADR-0013）。

## 背景

两个独立故障都出在 UI 线程的 `event_loop`（`crates/ys-tui-coding/src/run.rs`）。

**一、空闲 20 fps 无条件重绘。** 原循环每轮顶部无条件 `terminal.draw(...)`，
而等输入周期 `POLL_INTERVAL = 50 ms` ⇒ 即使没有任何事件、输出或输入，也固定以
20 fps 重算布局、滚动、`wrap` 并写终端。本机实测：debug 构建空闲约 9% CPU，
release 构建约 0.7%。release 的绝对值不大，但它是**常驻**的：agent 空闲时本应只
阻塞在「等下一个事件」上，重绘属于纯无谓开销。

**二、终端挂断后 crossterm 0.29.0 的 `poll` 自旋。** 终端消失后，stdin 的 `read(2)`
立即返回 EOF/EIO，而 `UnixInternalEventSource::try_read` 的内层循环不 break ⇒
`poll` 永不返回，主线程 100% CPU 且无法自行退出。本机实测到一个孤儿进程
（PPID 1、无控制终端）持续 **13h36m、累计 855 CPU 分钟**。该进程是孤儿进程组，
收不到 `SIGHUP`，因此不会自行退出。主线程卡在 crossterm 内部，自救不可行。

## 决策

### 1. 按需重绘：`FrameSchedule { dirty, last_status_second }`

帧的到期条件收敛为两个：

- **显式置脏**：`new` 即脏（保证启动后第一帧必定画出）；应用出站消息、按键 / 粘贴、
  浮层关闭返回、`Working` 节拍、状态行提示过期、贴边拖选，均调用 `mark()`。
- **秒数变化**：状态行含 `HH:MM` 与会话秒数，只需 1 Hz；`frame_due(now_secs)` 发现
  当前秒数变化时自动置脏。

`frame_due` 读取并清除脏标记；只有到期才执行滚动夹取与 `terminal.draw(...)`。
**绘制与滚动夹取放在 `poll` 之前结算**：出站新内容与启动首帧不必多等一个 poll 周期。
事件处理本身只置脏、不直接画 —— 下一轮开头会立刻画出，省掉一次多余的 poll 等待。

**`POLL_INTERVAL` 保持 50 ms 不变。** outbound 未并入 `poll` 的等待集，循环形状是
「先排空 outbound、再 poll」；把超时调长会推迟对出站消息的感知（流式输出变卡），
调短则纯增空转唤醒。按需绘制已经消除了空闲重绘，超时无需再动。

### 2. unix 终端挂断看守线程

`run()` 在 `setup_terminal()` 之后启动 `#[cfg(unix)]` 的
`spawn_tty_hangup_watchdog()`：

```
loop {
    libc::poll(fd = 看守 fd（见下）, events = POLLIN, timeout = 1000ms)
    if revents 含 POLLHUP | POLLERR | POLLNVAL {
        restore_terminal_on_panic();   // best-effort
        process::exit(0);
    }
    if 普通 POLLIN 醒来 { sleep(50ms); }   // 本线程不读 stdin，限速
    // n < 0：仅 EINTR 立即重试；其他持续性错误 sleep(50ms)
}
```

- **看守 fd：按 0 → 1 → 2 找第一个 `isatty` 为真的 fd，用 `F_DUPFD_CLOEXEC` 复制一份。**
  三者都不是 tty 时返回 `None`、不启动线程。理由有三层：
  - 必须盯**与 crossterm 读取的同一个终端**：硬编码 fd 0 时，`echo x | ys-coding-agent`
    的 stdin 是管道，写端关闭后 fd 0 读到 EOF ⇒ 终端仍健康却被误判挂断并退出。
  - **不能 `open("/dev/tty")`**：Darwin 上新打开的 `/dev/tty` fd 在 `poll` 里恒报
    `POLLNVAL`（32），mio / kevent 注册报 `EINVAL`（22）；`select` 正常。实测：

    ```
    poll(dup(0), POLLIN, 300ms)           -> []          # 正常超时
    poll(open("/dev/tty"), POLLIN, 300ms) -> [(3, 32)]   # POLLNVAL
    mio register(open("/dev/tty"))        -> EINVAL (22)
    dup(1) 关闭 master 后 poll            -> [(3, 17)]   # POLLIN|POLLHUP
    ```

    因此只能 dup 一个**已经打开**的 tty fd；0/1/2 通常与 `/dev/tty` 指向同一终端
    （极窄例外见 Consequences「已知边界」），且 `poll` 能正常上报 `POLLHUP`。
  - 用 `F_DUPFD_CLOEXEC` 而非普通 `dup`：看守 fd 活到进程结束，普通 `dup` 不带
    `FD_CLOEXEC`，会被之后启动的所有工具 / 后台命令子进程继承。
- 判定抽成纯函数 `tty_hung_up(revents)`，`POLLHUP` / `POLLERR` / `POLLNVAL` 任一置位
  即视为终端已消失。实测挂断的主信号是 `POLLHUP`（`revents = POLLIN | POLLHUP = 17`）；
  `POLLNVAL` 只在 fd 无效 / 已关闭时出现。
- `n < 0` 时仅 `EINTR` 立即重试；其他持续性错误 sleep 50 ms 再重试，避免把「等待」
  退化成 100% 自旋。
- 普通输入会让 `poll` 返回 `POLLIN`；本线程**不读 stdin**，若立即重试会在输入突发 /
  粘贴 / 鼠标事件期间空转，故睡 50 ms 把唤醒上限压到 20 次/秒。挂断仍由下一次
  `poll` 捕获，最坏多等 50 ms。
- 恢复用 `restore_terminal_on_panic()` 是 best-effort：写向已死的 tty 无副作用，
  失败也不阻塞退出。退出码 0 —— 终端消失不是应用层失败。
- 线程不 join，生命周期即进程；阻塞在 `poll` 上，成本可忽略。

### 3. Darwin 的 `poll(2)` 约束：`events` 必须含 `POLLIN`

macOS 实测：**未请求 `POLLIN` 时 `poll` 不上报 `POLLHUP`**。pty 关闭 master 与 pipe
关闭写端两种情况下，`events = 0` 都只会超时返回 0；改为 `events = POLLIN` 才返回
`revents = POLLIN | POLLHUP`（数值 17）。因此看守线程的 `events` 固定为 `POLLIN`，
不能为了「只等挂断」而写成 0。本仓主平台是 macOS，以实测为准。

### 4. 模态浮层循环本次不改

`run_prompt` 等模态循环仍是 50 ms 轮询重绘，只在浮层打开时运行。它不属于空闲稳态
（浮层是用户正在交互的临时态），记为后续项，不与主循环的按需重绘绑定。

## Considered Options

- **保留无条件重绘，只把节拍降到 100 ms**：CPU 减半但仍常驻，且没有表达「没有变化就
  不该画」。否决。
- **只按事件 / 出站重绘，不做 1 Hz 秒数刷新**：状态行的钟点与会话时长会停住不走。
  要么另起定时线程，要么接受文字冻结；前者比一次秒数比较复杂得多。否决。
- **把 outbound 并入 `poll` 等待集（self-pipe / 独立输入线程），让 `poll` 无限阻塞**：
  能彻底去掉超时唤醒，但要重构出站等待机制。当前收益不足以抵消改动面，留作后续，
  不是永久否决。
- **处理 `SIGHUP`**：本例是孤儿进程组，收不到 `SIGHUP`。否决。
- **打开 `/dev/tty` 作为看守 fd**：Darwin 的 `poll` 对新打开的 `/dev/tty` 恒报
  `POLLNVAL`、mio 注册报 `EINVAL`，会立刻误报挂断。否决，见决策 2 的实测。
- **vendor / patch crossterm**：等上游修复后移除看守线程是退场路径；当前维护私有
  patch 的成本不抵收益。否决。

## Consequences

- **空闲 CPU**：稳态只剩 50 ms 一次的 `poll` 与 1 Hz 的状态行重绘，不再每轮做布局、
  滚动与 `wrap`。修复前实测空闲约 9%（debug）/ 0.7%（release）。
- **流式延迟不变**：`apply_outbound` 置脏，下一轮在 `poll` 之前就画；不额外增加帧延迟。
- **状态行精度**：`HH:MM` 与会话秒数最多 1 秒更新一次，符合其展示粒度；需要更细时间
  读数的地方走 debug / 日志，不走屏幕。
- **终端挂断**：最迟约 1 秒（若刚有普通输入则再 +50 ms）恢复终端并 `exit(0)`；
  不再产生 100% CPU 的孤儿进程。
- **挂断退出的代价**：`process::exit(0)` 会跳过正常收摊（app 线程 grace shutdown、
  回合末会话落盘），最坏丢失最后一批尚未落盘的状态。此时终端已消失，这个代价可接受。
- **线程与依赖**：unix 下多一个阻塞在 `poll` 的看守线程；`libc` 已是 unix 目标的既有
  直接依赖，无新增依赖。
- **已知边界**：0 → 1 → 2 的假设建立在「crossterm 能正常初始化」上。stdin 非 tty 时，
  crossterm 0.29 在 Darwin 上会因 mio 注册 `/dev/tty` 报 `EINVAL` 而以
  `Failed to initialize input reader` 失败（上游既有行为，与看守线程无关），该配置下
  TUI 本就无法运行。stdout / stderr 指向「另一个 tty」的极窄场景，看守线程可能盯错
  终端；接受。
- **未覆盖项**：模态浮层仍 50 ms 轮询重绘（见决策 4）。
- **测试**：`FrameSchedule` 的纯函数用例覆盖「新建即脏」「显式置脏」「秒数变化触发」
  「无变化不画」；`tty_hung_up` 覆盖三个挂断位及与 `POLLIN` 的组合。看守线程本身依赖
  真 tty，不做单测。

## 关联

- [ADR-0013](0013-cancel-token-removal-boundary-source.md) — 三信道 / 两线程拓扑（本 ADR 不改）
- [ADR-0051](0051-tui-text-selection.md) — 贴边拖选的 `DRAG_TICK = 16 ms`（仍是本循环的输入周期）
- [ADR-0067](0067-quiet-run-reading.md) — 状态行 `HH:MM` 与会话时长；其决策 2 中
  「每 50 ms 无条件重绘」的前提由本 ADR 取代
- `docs/design-final/coding-agent-tui.md` §2 — as-built 循环形状
- `docs/architecture.md` §2 — UI 线程节点描述
