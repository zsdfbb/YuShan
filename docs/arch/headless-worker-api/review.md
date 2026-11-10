# headless-worker-api — 架构质量分析报告

> 日期：2026-11-10
> 对象：[`design.md`](./design.md) + [`adr-001`](./adr-001-headless-worker-contract.md)
> 方法：逐条核对设计中引用的代码指针（`wiring.rs` / `main.rs` / `basic.rs` / `turn_advance.rs` / `post_turn.rs` / 各测试）

---

## 分析范围

- **对象**：设计方案（非已实现代码）
- **维度**：全量
- **重点**：设计里每一条「代码事实」断言是否成立 —— 因为本设计的全部论证都建立在它们之上

---

## 各维度判断

| 维度 | 判断 | 关键发现 |
|---|---|---|
| 1.1 技术可实现性 | 🟢 | 三件改动都是局部接线，无算法风险。`select!` 分支、一行 `println!`、一个 `match` 分支 |
| 1.2 依赖成熟度 | 🟢 | 只加 tokio `signal` feature（已在 `Cargo.lock`，无新下载） |
| 1.3 实现周期 | 🟢 | 路径清晰。R1（golden 修法错误）已在 design §7 修正 |
| 2.1 模块边界 | 🟢 | 全部落在 `apps/coding-agent`，`crates/` 零改动 —— 这是本设计最强的性质 |
| 2.2 接口稳定性 | 🟡 | `--json` 契约变更 + 退出码不兼容，但都是**有意的**、已标出。R5 已写进 design §5.1/§5.2 |
| 2.3 错误处理 | 🟢 | 退出码三档分工清楚；R6 已在 design §5.3 改为穷举写法 |
| 2.4 并发安全 | 🟢 | 工具的 abort 轮询链路完整（100ms 上界）；剩余风险在 `run_json_mode` 的 `join!` 收尾路径（R2） |
| 2.5 资源管理 | 🟢 | `terminate()` 杀整个进程组（`bash.rs:386-398`），孙进程不漏；`kill_on_drop` 兜 drop 路径 |
| 3.1 概念一致性 | 🟢 | 「退出码答能不能重试，`stop_reason` 答为什么」这条分工很干净，且与 `goal-command` 不撞码 |
| 3.2 上手成本 | 🟢 | 三个判据 G1–G4 与三条改动一一对应，读一遍就能动手 |
| 4.1 性能模型 | 🟢 | 无要求，判断正确（一行输出 + 一个 `select!` 分支） |
| 4.2 故障模式 | 🟡 | 「有 id 无文件」已显式接受；R5 的「信号落在回合刚完成后 ⇒ `0`」已归类写明 |
| 4.3 可观测性 | 🟢 | 首行 session 记录本身就是可观测性改进 |

---

## 处置状态（2026-11-10 已全数处理）

| 风险 | 处置 | 落点 |
|---|---|---|
| R1 golden 修法 | ✅ 改为扩展 `without_round_usage`，golden 保持 4 行 | design §6 #5 / §7 |
| R2 收尾路径 | ✅ 信号只 `push(Abort)`，不新增退出路径 | design §2 #4 / §5.2 / §8、adr-001 |
| R3 信号范围 | ✅ `signal()` 建在 `loop` 之前；明确不需额外判据 | design §5.2 |
| R4 `--resume` id 回显 | ✅ 补测试用例 + 回显理由 | design §5.1 / §9 |
| R5 输出边界 | ✅ 首行晚于 `ensure_configured`；完成竞态归 `0` | design §5.1 / §5.2 |
| R6 静默兑底 | ✅ 改穷举 `match` | design §5.3 |
| R7 文档漂移 | ✅ 列入改动面 #4 | design §6 |

**剩余 🟡 均为有意的不兼容契约**（`--json` 首行、退出码 `75`），已写进 README 同步项，
**不是未解决偏差**。⇒ 无阻塞开工的未决项。

---

## 核实通过的断言（设计站得住的部分）

逐条查了代码，以下全部成立，**不需要改**：

| 设计的断言 | 核实结果 |
|---|---|
| id 在构造期即已知，不需饿汉落盘 | ✅ `wiring.rs:148` `path.unwrap_or_else(...new_session_id())` 立即算；`:152` `JsonlSession::open` 不建文件；`:311` `session_path()` 返回 `Option<&Path>` |
| `Abort` 探针在追加用户消息之前 | ✅ `basic.rs:496` 探针 → `:518` 才 `append` |
| 两条模式共用 `consume_events`，但写回调不同 | ✅ `:2429` `run_print_mode` 与 `:2596` `run_json_mode` 各自 `join!`，回调不同 |
| `--json` 唯一 stdout 写点是 `write_json_envelope` | ✅ `:579`，仅 `:2600` 一处引用 |
| tokio 缺 `signal` feature | ✅ `Cargo.toml:29` `["rt-multi-thread","macros","io-util","fs","sync","time"]` |
| `single_turn_exit_code` 现在 `Cancelled\|NoOutput\|Failed ⇒ 1` | ✅ `main.rs:2689-2696` |
| boundary 是 `run_single_turn` 局部量，不外泄 | ✅ `main.rs:2086` |
| 预算计时器就在现成的 `select!` 里 | ✅ `main.rs:2129-2154` |
| `-p`/`--json` 的早退（无配置）不输出首行 | ✅ `run_json_mode` 第一句就是 `ensure_configured(...)?` (`:2564`)，先于任何输出 |
| `envelopes()` / `parse_all_json_lines()` 只要求「每行是合法 JSON」 | ✅ `debug_log.rs:553-560`、`budget_exit.rs:378-389` —— 新行能过 |
| 「stdout 只应有 Envelope」会红 | ✅ `debug_log.rs:1041-1044` 断言 `value.get("event").is_some()` |
| `sessions.rs` 按目录扫 `*.jsonl` | ✅ `:48-60`，所以「不饿汉落盘」避免空条目进列表这条理由成立 |

**另外一个设计没写、但对它有利的事实**（见 R3）：`turn_advance::advance` 对 `Cancelled` 返回
`Stop(Aborted)`（`turn_advance.rs:294-304` 有单测钉住），且 `post_turn::should_follow_up` 只在
`Completed` 时放行（`post_turn.rs:82-85`）。⇒ 信号产生 `Cancelled` 后，**多回合 loop 会自己停**，
不会起追问回合。设计的结论对，但依据是**巧合**，文档里没有这条引用。

---

## 风险排序

### R1（P0）§7 的 golden 修法是错的 —— 不能「4 行改 5 行」

`design.md:195` 说 golden「4 行基线要变 5 行」。**做不到**，而且会破坏该测试的立身之本。

`tests/debug_log.rs:876-919` 的 `write_baseline_golden` 要求用**基线二进制**
（`YUSHAN_BASELINE_BIN`，`7987b4b` 构建）重新生成 golden。基线二进制**不会**输出
session 行 ⇒ 重生成的仍是 4 行。而新二进制输出 5 行，比对恒失败。

**正确的修法**：改过滤器，不改 golden。`without_round_usage`（`debug_log.rs:567-584`）
现在按 `event.RoundUsage` 丢行；session 行**没有 `event` 字段** ⇒
`is_round_usage` 为 false ⇒ 被保留。给它加一条：**没有 `event` 字段的行也丢掉**
（正是「非 `Envelope` 行」的定义）。

这样 golden 保持 4 行逐字节不变，该测试继续回答它本来的问题
（「过滤掉本设计不关心的行后，输出与基线一致」），语料仍来自**未改动的基线二进制**。
这是 §7 唯一一处会导致实现卡住的错误。

### R2（P1）信号收尾不要另开路径 —— `join!` 期间 `debug` sink 被独占

⚠️ **本报告初稿在此处错了，已撤销。** 初稿曾声称「工具调用不可中断 ⇒ 5 秒宽限无法兑现、
会留孤儿」。核实后事实完全相反：

| 工具 | 中止链路 | 杀进程组 |
|---|---|---|
| `bash` | `wait_interrupt` 每 **100ms** 轮询 `is_aborted()`（`bash.rs:110-118`、`:535-565`） | ✅ `terminate()`（`:386-398`）→ `kill -9 -{pid}`（`proc_group.rs:25`） |
| `exec` 类（rg/fd/grep/find） | 主循环每 100ms 探（`exec.rs:326`） | ✅ `:337-347` → `kill_group_async` |
| `websearch` / `webfetch` | `wait_aborted` 与 chunk 循环赛跑（`net.rs:285-305`） | —（纯网络，无子进程） |
| `subagent` | `ChildBoundary` 透传父 `Abort`（`subagent.rs:156-157`） | — |

且 `-p`/`--json` **确实挂载 boundary**（`main.rs:2086-2088` → `ports_with_boundary(Some(..))`）。
⇒ §5.2 的前提成立：`push(Abort)` 后 100ms 内就能杀掉工具进程组（含孙进程），
5 秒宽限留了 50× 余量。**这条不需要改设计。**

**真正要写进设计的是收尾路径的独占问题**（`--json` 才有）：

```
let (run_result, consume_result) = tokio::join!(
    run_single_turn(… → push(Abort), debug_handle),      // 只持可克隆句柄
    consume_events(rx, stdout, …, debug.as_deref_mut()), // ★ 独占 &mut DebugSink
);
```

`exit_with_run_end` 需要 `&mut Option<DebugSink>`（`main.rs:2725`），而 `join!` 期间它被
`consume_events` 独占。正确结论是：**信号分支只需 `push(Abort)`，不要自己退出** ——
`join!` 会自然返回（回合以 `Cancelled` 收场 + 消费循环见终局事件即终止），随后走
`single_turn_exit_code` 得 `75`，全程不需要额外标志。

设计必须把这条写成明文：否则实现者很可能以为「信号得自己早退」而去抢 sink，
或另开一条 `process::exit` —— 那正是 `main.rs:2720` 注释警告的「任何新退出路径都必须走本函数」。

### R3（P1）信号的**范围**没写 —— 设计只覆盖了单回合

§4.4 与 §5.2 通篇只讲 `run_single_turn` 的 `select!`。但 `-p` 与 `--json` **都是多回合 loop**：
`run_print_mode`（`:2404`）与 `run_json_mode`（`:2568`）各有 `loop`，每轮末尾
`advance_after_turn` 决定是否起追问回合。

两个随之而来的问题，设计都没回答：

1. **`signal()` 建在哪一层**？§5.2 说「在两个机器模式各建一次，以 `Option<&mut Signal>`
   传入 `SingleTurnInputs`」。但 `SingleTurnInputs` 是**每回合新建**的（`:2429`/`:2595`），
   若 signal 也在每回合新建，则**回合之间**到达的 `SIGTERM` 会落在没有 `recv` 在等的窗口里。
   正确做法：在 `loop` **之前**建一次，循环内 reborrow 传入。
2. **loop 会不会再起回合**？结论是不会（R3 开头引的 `advance` → `Stop(Aborted)`，
   且 `should_follow_up` 只认 `Completed`），但**设计必须把这条依据写进来**，
   否则实现者按字面读会以为还要在 loop 层自己加标志位 —— 那会写出与
   `turn_advance` 重复的第二份判据，正是 `turn_advance.rs:6-8` 模块头警告的「两份条件必然漂移」。

### R4（P1）`--resume` 时首行必须回显**同一个** id —— 未明说、无测试

G1/G2 的真实闭环是：spawn → 拿 id → 存 → 下次 `--resume <id>` → **再拿一次 id**。
如果每次 spawn 都报新 id，bridge 的持久化值就会漂。设计 §5.1 只说「id 来源 =
`session_path()` 的 stem」—— 逻辑上 `persistent_at(Some(path))`（`wiring.rs:130`）
会把 `session_path` 设为传入路径，stem 天然是同一个 id，**行为是对的**。

但这是**隐式推论**，没有测试钉住。补一条：`--json --resume <id>` 的首行 id == `<id>`。
不加这条，将来有人「优化」成 `new_session_id()` 也不会被现有测试抓到。

### R5（P2）两个未明的输出边界

1. **首行必须在 `ensure_configured` 之后**。§5.1 说「进入回合循环之前」，没说相对
   `ensure_configured`（`:2564`）的位置。若写在它之前，无配置早退也会打出一个
   指向不存在会话的 id，bridge 会把垃圾 id 存下来。
2. **「信号到，但回合恰已完成」**。`SIGTERM` 落在 `turn` 刚返回 `Completed` 之后、
   `single_turn_exit_code` 之前 ⇒ 退出码 `0`。对 bridge 而言这是「成功了只是慢」，
   **可接受**，但设计应写明这条竞态归类为 `0`，别让实现者加额外的 `signal_fired` 标志
   去强改退出码（那会造成「明明跑完了却报 75」的假失败）。

### R6（P2）`_ => 0` 的静默兜底

`StopReason` 是 `#[non_exhaustive]`（`post_turn.rs:80-81` 注释明说）。改后的
`single_turn_exit_code` 仍是 `_ => 0` ⇒ 未来新增一个「失败类」变体会**静默退 0**。
现有设计也这样，不算新债，但三档设计既然把「能不能重试」当契约，值得加一条
穷举式 `match`（列出六个变体，让新变体触发编译提醒）。

### R7（P2）文档漂移（既有）

`docs/design-final/turn-output-and-feedback.md:48` 写「`-p` 与 `--json` 两处都经
`exit_with_code` 落地」——实际函数名是 `exit_with_run_end`（`main.rs:2725`）。
顺手修掉。

---

## 改进建议

### 易修复（低风险）

- **R1**：把 §7 的 golden 一行改成「扩展 `without_round_usage`：丢弃**无 `event` 字段**的行」，
  golden 保持 4 行不变。
- **R3**：§5.2 补两句 —— ①`signal()` 建在 `loop` 之前、循环内 reborrow；
  ②引 `turn_advance.rs:294-304` + `post_turn.rs:82-85` 说明 `Cancelled` 后 loop 自己停，
  **不需要**新增标志位。
- **R4**：§9 测试矩阵加一行「`--json --resume <id>` 首行 id == `<id>`」。
- **R5**：§5.1 写明「在 `ensure_configured` 之后、`loop` 之前」。

### 需讨论（要拍板）

- **R2**：§5.2 补一段：信号分支**只** `push(Abort)`；`join!` 会自然返回，收尾与退出码
  全交既有路径，**不新增退出路径、不加额外标志**。（前稿「工具杀不掉」的担心不成立。）

### 架构级（本轮不做）

- 无。（前稿列在这里的「前台工具可取消性」**不成立**：工具已有 100ms abort 轮询与
  杀进程组，无需新需求 —— 已核实撤销。）

---

## 总体评价

**整体健康度：良好。** 设计的主干是对的，而且判断力相当好 —— 拒绝往 `Envelope` /
`AgentEvent` 加字段（省下 ~200 处匹配）、拒绝指定会话 id（省下 id 清洗与路径逃逸）、
拒绝饿汉落盘（省下一整块无效设计），这三处「不做」比做对更值钱。改动面确实收在
`apps/coding-agent` 一个 crate 里，`crates/` 零改动，这是可以放心动手的形态。

**最大风险点：R1。** golden 的修法写错了，实现第一步就会卡住，且错误修法会毁掉
那个测试的立身之本（拦住改动前后的输出漂移）。纯文档修正，但必须先改。

R2 性质不同：它不是缺陷，而是**防护** —— 机制本来就成立，设计只是没把
「为什么不需要额外标志」写出来，容易被实现者改成第二条退出路径。

**推荐下一步**：R1–R7 已全部落入 `design.md` / `adr-001`，可进 `sequential-workflow`。
