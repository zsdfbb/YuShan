# Termux 下 subagent 派发导致「环境重启」—— 原因排查记录

- **日期**：2026-10-27
- **性质**：**排查记录，不是验收报告**。代码侧的成因全部有指针；系统侧的机制属**待现场验证的假设**（见 §5）。
- **触发**：用户报告「Termux 环境里 coding-agent 经常导致当前环境重启，尤其在用 subagent 时」。
- **结论索引**：最可能是 **Android 12 的 phantom process killer 对整个进程组发 `SIGKILL`**（§2.1），
  代码侧的四条放大因素见 §2.2–§2.5。

---

## 1. 现场事实（实测于本机，2026-10-27）

| 项 | 值 | 获取方式 |
|---|---|---|
| OS | Android **12**，SDK **31** | `getprop ro.build.version.release` / `.sdk` |
| 机型 | `WGRR-W19` | `getprop ro.product.model` |
| 核数 | **8** | `nproc` |
| 内存 | `MemTotal` **11,797 MiB**；`MemAvailable` **6,366 MiB** | `free -m` |
| Swap | 4,095 MiB，**已用 1,899 MiB** | `free -m` |
| cgroup 可见性 | `memory.max` / `memory.events` **读不到**（`/sys/fs/cgroup/*` 无权限） | 直接 `cat` 均空 |
| 本进程 `oom_score_adj` | **0**（前台态） | `/proc/self/oom_score_adj` |
| `ulimit -a` | stack 8 MiB、processes **42,998**、nofiles 32,768、其余 unlimited | `ulimit -a` |

两条对判断重要的观察：

1. **系统侧内存余量尚可，但 swap 已用掉近半** ⇒ 平时就在压力边缘；RSS 尖峰比「稳态」更容易触发低内存回收。
2. **`oom_score_adj` 目前是 0，只因为进程在前台**。切后台（锁屏 / 换 App）时 Android 会把后台进程的
   adj 一路抬到 900+，那时 Termux 排在**第一批**被回收的名单里。

---

## 2. 代码侧的放大因素（全部有指针，均已核实）

### 2.1 system 侧：Android 12 phantom process killer（**主要嫌疑**）

Android 12 起系统限制 App 派生的「phantom process」（App 自身不追踪的子进程）数量，默认上限 **32**；
超限时**杀整个进程组**，用户侧表现为 Termux 打出 `Process completed (signal 9) - press enter`、
当前 shell 连同跑在里面的东西一起没了 —— 即用户描述的「环境重启」。

**代码侧把进程数推到上限的路径**（三处叠加）：

1. **每回合可派发 16 次**，且 `subagent_async` **立即返回**、
   `is_concurrency_safe() == true` ⇒ **一批里就能同时起满 16 个子 agent**：
   - `apps/coding-agent/src/subagent.rs:33` —— `DEFAULT_MAX_DISPATCHES_PER_TURN: u32 = 16`
   - `apps/coding-agent/src/subagent.rs:1418`（`SubagentTool`）/ `:1505`（`SubagentAsyncTool`）
     —— 两者都声明并发安全
2. **每个子 agent 是一套完整运行时**，不是轻量任务：
   - `apps/coding-agent/src/subagent.rs:980` —— 每次派发都 `toolset::build_tools(...)`
     重建 **9 件套工具表**（`apps/coding-agent/src/toolset.rs:88`）
   - 每个子 agent 自己的 `MemorySession` + `CollectingSink` + `BoundarySource`（`:1028`–`:1032`）
3. **每个子 agent 的 `bash` 都是独立进程组**，`grep`/`find` 再 fork `rg`/`fd`：
   - `adapters/tools-basic/src/bash.rs:497` —— `cmd.process_group(0)`
   - `adapters/tools-basic/src/bash.rs:479` —— `sh -c <cmd>`
   - **额外**：每次杀组又 fork 一个 `sh` —— `adapters/tools-basic/src/proc_group.rs:26`
     （`kill_group` 里 `Command::new("sh").arg("-c").arg("kill -9 -{pid}").status()`）

⇒ 峰值进程数量级 ≈ `N子agent × (1 组长 + M 孙进程)` + 父侧 `bash_bg` 注册表（`max_running: 4`，
`adapters/tools-basic/src/background.rs:301`）+ 杀组用的 `sh`。**N = 16、每个子 agent 再起两三个进程，
就贴着 32 这条线**；并行批装得满时越界，系统一次性杀掉整组。

### 2.2 **没有并发闸**（结构缺口，最值得修）

`LiveSubagents` **只有计数器，没有准入限制**：

- `apps/coding-agent/src/subagent.rs:347` —— `struct LiveSubagents { running: AtomicUsize, settled: Notify }`
- `apps/coding-agent/src/subagent.rs:362` —— `enter()` 直接 `fetch_add`，**不等待、不排队**

全仓源码 grep `Semaphore` **零命中** ⇒ 16 次派发是**同时**跑，不是一个一个来。
`max_dispatches` 限的是**次数**，不是**同时**数。

### 2.3 `bash` 单条输出**无界**累积（RSS 尖峰的直接来源）

- `adapters/tools-basic/src/bash.rs:511`–`519` —— stdout / stderr 各用
  `read_to_end(&mut buf)` **无上限**读进 `Vec<u8>`
- 截断发生在**之后**：`adapters/tools-basic/src/bash.rs:209` 的
  `ys_tool::tail(&combined, MAX_LINES, MAX_BYTES)`（50 KiB）

⇒ 单条 `cargo build` / `cat 大日志` / `rg` 结果可以先把几百 MB 读进内存，再从里面截出尾部 50 KiB。
**16 个子 agent 同时各来一条**，就是这个量级乘以 16。

对照：`grep`/`find` 走的是**有界**原语 `run_bounded`（`adapters/tools-basic/src/exec.rs:179`，
`byte_cap` = 1 MiB、达上限即杀），**`bash` 是唯一没有这条闸的那个**。

### 2.4 子 agent 的事件与转录**全量留在内存**

- `crates/ys-event/src/collecting.rs:8` —— `CollectingSink { events: Vec<AgentEvent> }` **无上限**
- `crates/ys-session/src/memory.rs:8` —— `MemorySession { messages: Vec<Message> }` **无上限**
- `apps/coding-agent/src/subagent.rs:1246` —— `transcript_body(...)` 把**整个会话**再拼成一条
  完整 `String`，之后才写盘

⇒ 一次派发的内存占用约等于「会话历史 × 3」（session + collecting sink 里的事件 +
写盘前的转录字符串），且**到派发结束才释放**。

### 2.5 线程数（次要，只放大上面几条）

- `apps/coding-agent/src/main.rs:2835` —— `tokio::runtime::Builder::new_multi_thread().enable_all().build()`
  ⇒ worker 数取 `nproc`（本机 8），**未设 `worker_threads` / `max_blocking_threads`**
- 每个子 agent 另起一个 watchdog 任务（`apps/coding-agent/src/subagent.rs:1021`）
- `std::thread::spawn` 的阻塞点：debug 写线程（`apps/coding-agent/src/debug_log.rs:397`）、
  `kill_group_async` 的 `spawn_blocking`

线程本身不会让系统杀进程，但它们会抬高 RSS 与调度压力。

---

## 3. 已排除 / 不像的方向

| 方向 | 判断 | 依据 |
|---|---|---|
| **栈溢出** | 不像 | `ulimit -s` 8 MiB（默认值）；子 agent 递归受 `ToolRegistry` 结构约束（子表无派发工具），不是深递归 |
| **文件描述符耗尽** | 不像 | `nofiles` 32,768；单次派发最多几个管道 |
| **进程数硬限 `ulimit -u`** | 不像 | 42,998，远超 32 ⇒ 撞的是**系统**的 phantom 上限，不是 `ulimit` |
| **`kill -9 -<pgid>` 误伤自己** | 不像 | 组 id 来自 `child.id()`（`bash.rs:504`），且 `process_group(0)` 让组长自成一组；父进程不在该组 |
| **`std::process::exit` 提前退出** | 不像 | 「环境重启」是整个 Termux 会话没了，不只是 agent 退出 |

---

## 4. 后果说明：为什么是「环境重启」而不是「agent 崩了」

`process_group(0)`（`bash.rs:497`）让每个 bash 组长自成进程组 —— 这**保护**了父进程不被
`kill -9 -<pgid>` 误伤，但也意味着 **phantom 计数是按 App 的整体子进程算的**：
子 agent 树的进程数照样计入 Termux 这个 App。系统做限制时**不区分**你是 harness 派发的还是用户敲的
⇒ 一次超限，整个 Termux 会话（含 shell）一起被端掉。

---

## 5. 待现场验证（**没有这些证据前，§2.1 仍是假设**）

1. **看 Termux 有没有打出 `signal 9` / `singal 9`** —— 有则基本确认是系统杀的
   （Android 的进程被杀与用户态 panic 在表现上可区分：后者有 backtrace）
2. **量进程数峰值** —— 跑一次多 subagent 任务，另开窗口
   `while :; do echo "$(ls /proc | wc -l)"; sleep 1; done`
   若峰值逼近/超过 32，§2.1 成立
3. **量 RSS 峰值** —— `ps -eo rss,comm | sort -nr | head`，观察 `ys-coding-agent` 是否出现数百 MB 以上的台阶
4. **看 adj 是否被抬高** —— `cat /proc/<pid>/oom_score_adj`，锁屏或切后台后再看一次
5. **系统侧旁证**（需 root / adb，本机不可用）：`dmesg`、`logcat` 里的 `lowmemorykiller` /
   `phantom` 记录；`settings get global settings_enable_monitor_phantom_procs`

> ⚠️ 本机 `/sys/fs/cgroup/memory.events` 与 `dmesg` 均读不到 ⇒ **无法在 Termux 内部确认是否发生了
> OOM kill**，只能靠外部（adb / root / 用户观察 `signal 9`）。这一点决定了 §2.1 与 §2.3 谁是主因。

---

## 6. 候选修法（**尚未实施**，按性价比排序）

| # | 修法 | 作用点 | 影响面 / 代价 |
|---|---|---|---|
| R1 | **给在跑子 agent 加全局准入闸**（`Semaphore`，如 4），超出的**排队**而不是同时起 | `apps/coding-agent/src/subagent.rs:347`（`LiveSubagents`） | 直接砍进程数峰值。改动 `enter()` 为 `async` ⇒ 派发路径变 async；语义从「最多 16 次派发」变为「最多 16 次派发、最多 N 并发」⇒ **需补 ADR**（改动 ADR-0022 决策 15 / ADR-0055 的语义边界）+ 变异测试（闸失效 ⇒ 并发数断言变红） |
| R2 | **`bash` 输出改成有界收集**：累积到 N MiB 后继续读并丢弃，只置标志 | `adapters/tools-basic/src/bash.rs:511` | 复用 `exec.rs::run_bounded` 的既有语义（`exec.rs:179`），行为对模型**可见变化仅一条截断通知**。**无需 ADR**，属实现收敛；需补「达上限仍读到 EOF」的用例（照 `grep.rs:1037` 的先例） |
| R3 | **`CollectingSink` 只留必要事件**（子 agent 消费方只用到 `ToolResult` 原文，见 `subagent.rs:1193`）+ 转录流式落盘 | `crates/ys-event/src/collecting.rs:8`、`subagent.rs:1246` | 砍掉约一半峰值。风险：sink 是通用类型，收窄会波及测试断言 ⇒ 宜新增受限变体而非改默认 |
| R4 | 显式设 `worker_threads` / `max_blocking_threads` | `apps/coding-agent/src/main.rs:2835` | 次要收益；Termux 上 worker 数降到 2–4 可省 RSS |
| R5 | **系统侧**（非代码）：关闭 phantom monitor 或给 Termux 加电池优化白名单 | 系统设置 / adb | 立刻见效但不属本仓；且**不该**作为本仓的解法 |

**建议顺序：R2 → R1 →（视 §5 结论）R3/R4。** R2 无契约变更、单独就能消掉最大的一类尖峰；
R1 才动设计语义，应走一次 `arch-design` 再落地。

---

## 7. 本次未做的事

- **未构造复现回路**：本机无法读 cgroup / `dmesg`，Termux 里也无法模拟「杀掉整个 App 进程组」
  而不真的杀自己 ⇒ 按 `diagnose` 的纪律，§5 的证据只能由用户侧提供，**不在此假设上直接改代码**。
- **未改任何代码**，未跑测试（本次结论全部基于静态核实 + 现场环境测量）。
