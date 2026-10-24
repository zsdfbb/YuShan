# test-build-speed — 架构上下文

> 日期：2026-10-23
> 性质：arch-explore 产出（现状测量 + 约束 + 已落地改动）。**不做方案设计**。
> 缘起（用户原话）：「优化一下 cargo test 的执行，现在跑一次太慢了，要好几分钟。」
> 后续追加：「能否将增量的构建和测试压缩到 10s？考虑 sccache、更快的链接器、cargo-nextest 等优化？」
> 「nextest 可以换国内源安装？将本项目的默认源改为国内源，比如阿里云」

**与既有文档的关系**：`docs/arch/explore-speed/` 测的是**模型探索**的墙钟（工具调用轮数），
`docs/arch/execution-efficiency/` 是**运行时**的后台命令执行。本文件只回答
「**开发者的编辑-测试循环**慢在哪、哪些杠杆真的能动它」。

---

## 概述

「`cargo test` 要好几分钟」在 8 核 Android（Termux, aarch64）上实测拆开为三段：
**构建 / 链接**、**测试二进制执行**、**联网**。结论先行：

1. **热缓存下全量测试只需 34s，不是几分钟**。所谓「几分钟」的观感来自两个真实但不同的原因：
   - **`cargo test` 串行执行各个测试二进制**（`tests/*.rs` 逐个跑），总耗时 = 各二进制之和。
     用例本身早已在二进制内部并行（libtest 默认 `test-threads` = 核数）。
   - **首次/冷构建**与**联网**（官方 crates 源在本网络下 15s 即超时）。
2. **最大的单项杠杆是跨二进制并行**：34s → **7.4s**（同 1555 个用例，已核对 1:1 覆盖）。
   **已定为全量测试的唯一入口**：`./scripts/par-test.sh`（`AGENTS.md` 已列为硬要求）。
3. **mold / sccache / 削减 debuginfo 的收益都在噪声级**，因为瓶颈是 `rustc` 编译本身，不是链接。
4. **cargo-nextest 在本平台反而更慢**（14.1s）：它给每个**用例**起独立进程，而 Android 的
   fork/exec 比 Linux 贵得多。它的价值（隔离、重试、慢测试报告）与它的代价需要分开看。
5. **换国内源解决的是「卡住」而非「变慢」**：官方源在本网络下直接超时，
   `cargo install nextest` 连失败 3 次；换阿里云后同一命令一次成功（352s，零网络错误）。

---

## 现有架构

### 测量点与工具

| 工具 | 位置 | 作用 |
|---|---|---|
| `scripts/par-test.sh` | 本仓 | 取出全部测试二进制，跨二进制并行执行（默认 8 路） |
| `scripts/build-baseline.sh` | 本仓 | 冷 / 改底层 crate / 改叶文件 / 无改动 四场景构建计时 |
| `cargo nextest` | `~/.cargo/bin` | 每用例独立进程的测试运行器（已装，非默认） |

### 实测数据（8 核 aarch64-linux-android，1545 用例）

| 场景 | 耗时 |
|---|---|
| `cargo test --workspace`（串行二进制，热缓存） | **34s** |
| `./scripts/par-test.sh`（8 路，同用例） | **7.4s** || `cargo nextest run --workspace`（每用例一进程） | **14.1s** |
| `cargo test --workspace --lib`（只 881 个单测） | 6s |
| 无改动构建（noop） | 0.32s |
| 改 `ys-core`（底层，几乎所有 crate 依赖它）后重建 | **11.0s** |
| 改 `apps/coding-agent/src/app_loop.rs`（叶文件）后重建 | **3.6s** |
| 冷构建（空 target，`--tests`） | 75–176s |

### 已试过但没有收益的杠杆

| 杠杆 | 实测 | 判定 |
|---|---|---|
| **mold**（`-fuse-ld=mold`） | incr_core 11.0s → **10.75s**（−2%） | 链接不是瓶颈，**否决** |
| **sccache**（`RUSTC_WRAPPER`） | 增量 11.0s → **11.5s**；清空 target 后 176s → **108s** | 只对「清 target 重建」有效，日常增量被包装器开销抵消 |
| **`CARGO_INCREMENTAL=0`** | incr_core **51s**、incr_app **19s** | 显著更差，**必须保留增量** |
| **`debug = "line-tables-only"`** | incr_core 11.0s → **9.6s** | 有收益，但代价是调试信息变薄 |

### 已落地的配置与入口

| 文件 | 作用 |
|---|---|
| `scripts/par-test.sh` | **全量测试唯一入口**（跨二进制并行，默认 8 路，支持 `--jobs` / `--` 透传 / `--no-build`） |
| `scripts/build-baseline.sh` | 四场景构建计时，对比配置收益用 |
| `.cargo/config.toml` | 阿里云 sparse 镜像 + `net.retry = 5` |
| `~/.cargo/config.toml` | 同上（机器级）。**`cargo install` 只读这份**，不读项目级 `[source]` |

### 外部依赖

| 依赖 | 用途 | 备注 |
|---|---|---|
| `cargo-nextest` 0.9.149 | 可选的测试运行器 | 已装到 `~/.cargo/bin`（需自行加 PATH） |
| `mold` 3.0.0 | 链接器 | 已装，但**实测无收益**，未启用 |
| `sccache` 0.18.0 | 编译缓存 | 已装，未启用 |
| 阿里云 crates 镜像 | 索引 + 下载 | `sparse+https://mirrors.aliyun.com/crates.io-index/` |

---

## 约束

- **技术**：`Cargo.lock` 已提交（262 个包），版本本就锁定；换源**不改**锁文件内容
  （实测 `cargo fetch` 后 `Cargo.lock` 逐字节未变）。
- **技术**：`cargo install` **不读项目级 `.cargo/config.toml`** 的 `[source]` 段
  （它对应「安装目标」而非「当前项目」的依赖）—— 全局装 nextest 必须写 `~/.cargo/config.toml`。
- **性能**：编辑循环的**下限**由「改动的 crate 被多少 crate 依赖」决定。
  改 `ys-core` 必然 >10s（12 个 crate 重编），这是分层依赖的固有代价，不是配置能消除的。
- **组织**：项目无 CI，测试靠本地手动跑 ⇒ 快路径必须**零额外依赖**，不能要求装 nextest 才能跑测试。
- **环境**（非项目问题）：本机**后台 shell 有 64MiB 单文件写入上限**（`RLIMIT_FSIZE`），
  链接 130MB 的测试二进制会报 `File size limit exceeded` 而失败；前台 shell 无此限制。
  这曾让多次「后台跑构建」的测量结论失效，记录在此避免重复踩。

---

## 需求范围

### 范围内

- 让**日常全量测试**从 34s 降到 10s 以内（已达成：7.4s）。
- 修掉那些**在并行负载下偶发失败**的用例（它们会让人不敢用快路径）。
- 让**联网**不再成为冷构建的阻塞项（换阿里云源）。
- 把结论落成**脚本 + 配置 + 文档**三处，后续会话可直接用。

### 范围外（明确不做）

- **不改测试语义**去换速度（不删用例、不 merge 用例、不放宽断言判别力）。
- **不引入必须安装的工具**作为跑测试的前提（nextest 可选，非默认）。
- **不动分层**去缩短 `ys-core` 的增量重建（那是架构代价，不是构建配置问题）。
- **不启用 mold/sccache**（实测无收益，凭空多一个环境依赖）。

### 关键场景

1. **日常编辑循环**：改一个 app 层文件 → `./scripts/par-test.sh` → 约 11s（3.6s 构建 + 7.4s 测试）。
2. **改底层 crate**：改 `ys-core` → 约 18s（11s 构建 + 7.4s 测试），这是最坏情况。
3. **新机器 / 冷启动**：`git clone` → `cargo fetch`（阿里云，约 11s）→ 首次构建（约 75s，仅一次）。
4. **想定位慢测试**：`cargo nextest run --workspace` 给出每用例耗时。

---

## 本轮已落地的改动

1. **`scripts/par-test.sh`**（新增）— 跨二进制并行测试，默认 8 路。这是 34s → 7.4s 的
   直接来源。失败时保留完整输出，正常路径不刷屏。
2. **`scripts/build-baseline.sh`**（新增）— 四场景构建计时，用于以后对比构建配置收益。
3. **`.cargo/config.toml`**（新增）— 阿里云 sparse 镜像 + `net.retry = 5`。
4. **`~/.cargo/config.toml`**（新增，机器级）— 同样内容，供 `cargo install` 使用。
5. **修 `debug_log.rs` 的 `normalize_log` 真 bug**（见下）— 这曾让并行跑测试必然偶发失败。
6. **`exec.rs` 抽出 `poll_nap` 纯函数 + 确定性单测**，并放宽那条不稳定的墙钟断言。

### 顺带发现的两个真 bug

- **`normalize_log` 的归一化与数字位数相关**：它把 `round=` 行的**每个字符数字**替换成 `#`，
  于是 `ttft_ms=11` → `##`、`ttft_ms=2` → `#`，两份日志被判定成不同结构。
  只要真实 ttft 跨过 10ms 就误报「debug 开关改变了诊断输出」。已改为**按字段整体**归一化，
  并补了 `normalize_log_ignores_timing_digit_width`（变异测试确认：改回旧实现即变红）。
- **`test_run_bounded_timeout_converges_to_small_deadline` 的墙钟余量只有 50ms**
  （120ms deadline vs 170ms 上界），而变异版本约 200ms。并行负载下 tokio 定时器过冲即误报。
  已把收敛规则抽成 `poll_nap(deadline, elapsed)` 纯函数、用确定性单测钉住变异敏感性，
  端到端断言放宽到 400ms（仍能区分「收敛 ≈120ms」与「退化成固定睡眠 ≈200ms」）。

---

## 未澄清问题

- [ ] `debug = "line-tables-only"` 能再省约 1.4s 增量时间（11.0s → 9.6s），但会让 panic 栈
      只剩行号、无变量值。**待用户确认**是否接受这个调试体验降级。
- [ ] 是否要把 `par-test.sh` 设为**唯一**推荐入口（AGENTS.md 已把它排在 `cargo test` 前面），
      还是在 `Cargo.toml` 的 `[alias]` 里注册成 `cargo pt`？后者更顺手但会隐藏脚本路径。
- [ ] 目标态「增量 ≤10s」在**改底层 crate** 时做不到（11.0s 构建 + 7.4s 测试 = 18.4s）。
      **待用户确认**：10s 是指「只跑测试」还是「构建+测试」？
      若是后者，唯一的路是把 `ys-core` 的改动隔离（例如减少它的被依赖面），属架构级改动。

---

## 后续建议

- 建议用 `arch-design` 讨论「减少 `ys-core` 被依赖面」是否值得（当前 12 个 crate 依赖它，
  任何底层改动都触发大面积重编）。
- 若接受调试信息降级，把 `[profile.test] debug = "line-tables-only"` 写进根 `Cargo.toml` 即可。
- 慢测试定位：`cargo nextest run --workspace` 已在手边；若发现某用例 >2s，优先看它的
  `sleep` / 墙钟断言。

---

## 2026-10-24 追加探查（macOS / 解耦可行性）

> 环境：macOS arm64、10 核（4P+6E）、rustc 1.99。**本节的耗时数字尚未测**（见第 6 条）；
> 下面都是静态勘察结论与决策记录。

### 1. 构建缓存处于病态状态（必须先清理）

- `target` 总计约 **24 GB**；`target/debug/deps` 有 **932,030 个文件**。
- 其中 `*.rcgu.o` **925,912 个（约 22 GB）**，最早 2026-09-13、最新 2026-10-24；
  同目录另有 426 个可执行文件、1,010 个 `.rlib`、2,121 个 `.rmeta`。
- `.rcgu.o` 是链接前的中转对象，正常情况下会被 rustc 清理；成规模残留高度疑似
  「构建在链接/中途被杀死」。**测量前先 `cargo clean`**，否则基线不可信。
- 维护约定：长冷构建放前台并给足超时；不要让构建被超时 kill 后继续累积。

### 2. app 是最大的单一编译单元

- `apps/coding-agent`：src **30,176 行**（22 文件）+ `tests/` **9,730 行**（16 文件）；
  `apps/coding-agent/Cargo.toml` **没有 lib target**，只有 bin `ys-coding-agent`。
- `cargo test --no-run` 会把该 bin 编译两次（bin 目标 + bin test harness，428 个单测）；
  app 约一半行数是内联 `#[cfg(test)]` 测试。
- 近 6 个月文件级 churn：`main.rs` 54、`app_loop.rs` 37、`prompt.rs` 26、
  `crates/ys-loop/src/basic.rs` 25、`config.rs` 22、TUI `events.rs`/`app.rs` 各 20。

### 3. 测试目标与忽略用例

- `par-test.sh` 当前发现 **39 个测试二进制**；11 个 app 集成测试通过
  `env!("CARGO_BIN_EXE_ys-coding-agent")` 启动 34 MB 的真实二进制。
- `tests/e2e_tools.rs` 与 `tests/e2e_skills.rs` 各只有 1 个 `#[ignore]` 用例；
  `adapters/tools-web/tests/harvest.rs` 的 5 个用例全 `#[ignore]` —— 这三个文件
  适合用 `required-features` 移出默认构建。
- **`tests/e2e_bench.rs` 不能整文件移出**：它有 12 个 `#[test]`，只有 `bench_report`
  一个 `#[ignore]`，其余是活跃的解析/环境测试。要省只能把 `bench_report` 拆到新文件再 gate。

### 4. 解耦判断

- **值得拆**：`apps/coding-agent` 按依赖深度拆成 4 个产品 lib + 薄 bin
  （`ys-coding-base` → `ys-coding-domain` → `ys-coding-app` → `ys-coding-cli`）。
  高频编辑的 `main.rs`/`app_loop.rs` 会落进小得多的编译单元。
- **不值得拆**：
  - `ys-core`（504 行、churn 低，`Message` 几乎全仓使用；拆了收益小于成本）。
    这推翻了本文件前面「减少 `ys-core` 被依赖面」的后续建议：真正的高杠杆是 app crate。
  - `ys-tui-coding` 内部（`app ↔ commands ↔ completion ↔ draw ↔ events` 是密集环）。
  - 按事件域拆 `ys-event`（所有消费方都持有完整 `AgentEvent`）。
- **通用组件**（回答「哪些应该放 `./crates`」）：
  - `channel.rs`（666 行）是唯一现在就能干净下沉的通用件 →
    可选新增 `crates/ys-event-channel`（只是 `EventSink` 的 tokio 传输实现；
    `Envelope`/`LifecyclePolicy` 仍在零 tokio 的 `ys-protocol`；不是复活 ADR-0013
    删除的旧 `ys-channel`）。
  - `test_env.rs`（59 行）→ dev-only `crates/ys-test-support`。
  - `debug_log`/`round_clock`/`logging`、`wiring`、`provider`、`subagent` 仍带
    产品/adapter 耦合，列二期/三期；一期不硬拆。
  - `sessions`/`status`/`state`/`help`/`worktree` 与 `config`/`skills`/`prompt`/
    `toolset`/`assembly`/`capabilities`/`view`/`app_loop` 属产品层，留 app。

### 5. 构建/测试配置结论

- 根 `Cargo.toml` 目前**没有任何 `[profile]` 段**。
- 已确认 Cargo 语义：`[profile.dev.package."*"]` 只作用于**非 workspace 成员**
  （官方文档：for all dependencies (but not any workspace member)），因此可以只关
  第三方依赖的 debug，自己的 crate 保留完整调试信息。
- 冷构建由 248 个外部依赖主导；内部拆 crate 对冷构建帮助有限，主要收益来自
  「第三方依赖 `debug = false`」+ 清缓存。全量测试**执行**时间需单独测关键路径。

### 6. 待办：本机基线尚未测

plan mode 下不能跑构建，因此 **macOS 上的增量/冷构建/测试执行数字都还没有**。
下一步先按上面第 1 条清理 `target`，再用扩展后的 `scripts/build-baseline.sh`
测量，之后才能判断验收目标是否可达。

---

## 2026-10-25 本机基线已测（结算 §6 待办）

环境：macOS arm64、Apple M4 10 核、负载 5.26。全部为前台运行。

### 1. 清理前（病态）vs 清理后

| 场景 | 清理前 | **清理后** | 提升 | §4.2 文档基线（Android 8 核） |
|---|---:|---:|---:|---:|
| 冷构建 `--tests` | — | **24.96s** | — | 75–176s |
| noop | 0.35s | **0.24s** | — | 0.32s |
| 改叶子 `app_loop.rs` 后增量 | 4.68s | **0.98s** | 4.8× | 3.6s |
| 改 `ys-core` 后增量 | 163s | **4.20s** | 39× | 11.0s |
| `par-test.sh`（39 个二进制） | 184s | **7.55s** | 24× | 7.4s |

`cargo clean` 本身耗时 **92.39s**，移除 **967,797 文件 / 65.1 GiB**（`du` 报 24G 是严重低估）。

**判据证据**：清理前两个红字场景的 `user` / `sys` 严重失衡——改 `ys-core` 是
`user 15.09s` / `sys 104.75s`，测试执行是 `user 6.03s` / `sys 21.82s`。
真正的编译只占十几秒，其余全是**文件系统**在为 92 万个目录项付代价。

### 2. §4 的「值得拆 app」判断缺乏本机证据

清理后：改 `ys-core`（底层，12 个 crate 依赖它）**4.20s** vs 改 `app_loop.rs`（叶）**0.98s**，
差距仅 **3.2s**。把 30k 行 app 拆成 4 层能拿回其中一部分，但**同一份收益已被清理的 39× 覆盖**。
⇒ 拆分的立项理由（构建慢）在本机不成立。若要继续拆，须换理由（如两产品复用）。

**未测项**：§4 的拆 crate 方案未做实测对比，故「拆分无收益」是本机测量下的**阶段性结论**，
不是终局判定。

### 3. `.rcgu.o` 的真实成因：正常增量就在堆积（此条推翻 §1 的「疑似被 kill」）

| 实验 | `.o` 变化 |
|---|---|
| 连续 10 次改 `ys-core` | 13,357 → **75,827**（每次 **+6,247**） |
| 连续 5 次改 `app_loop.rs` | 75,827 → 78,387（每次 **+512**） |

**正常增量构建从不删旧的 `.rcgu.o`**，不需要「被 kill」。按 +6,247/次 推算，
92 万 ≈ 140 次 `ys-core` 级构建，即**数天正常开发量**，而非异常。

（§1 记的 `RLIMIT_FSIZE` 让链接失败，仍会加速堆积，但不是必要条件。）

**体积 vs 数量**：`.o` 逻辑大小 4.87 GB，但 `du` 报 `deps` 仅 1.2G —— APFS 对零填充的
`.o` 做了透明压缩。**瓶颈是文件数，不是字节数**（清理 92s 里 `user` 只有 0.72s）。

### 4. 清理方案实测（`cargo clean -p` 三个档次）

| 清理对象 | 耗时 | 条目变化 | 之后的构建 |
|---|---:|---|---:|
| 1 个 crate（`ys-coding-agent`） | **2.17s** | 51,938 → 49,632 | 6.92s |
| 4 个高 churn crate | 2.17s | 51,938 → 45,953 | 8.69s |
| **全部 14 个 workspace 成员** | **5.54s** | 81,500 → **928** | **16.04s** |
| 对照：全量 `cargo clean` | 92s | → 0 | 24.96s |

**为什么清全部成员**：清 4 个只降 6 千（未清的 crate 同样在堆）；清全部 14 个降到 928。
外部依赖（248 个包）**不清**——它们的重编才是冷构建成本的大头。

**手删 `.o` 也安全**（实测）：删 `ys_loop` 的 3,732 个之后，no-op 构建仍 0.08s、
二进制照常运行、`.o` 不被重建；下次真正重编时只补回当前需要的 **311** 个。
⇒ 一个 crate 正常只需 311 个，3,732 是约 12 次构建的累积。

### 5. 已落地：`scripts/cargo-build.sh`

构建/检查的统一入口（`AGENTS.md` 已列为改代码后的默认命令）：
先查 `deps` 条目数，超阈值（默认 **400,000**）就 `cargo clean -p` **全部 workspace 成员**
（包名从成员表派生，不硬编码），然后透传 cargo 子命令与参数、**退出码原样返回**。

- 检查成本 **0.11s**，故每次都查；清理成本 5.54s + 下次构建多付 11.84s（4.20 → 16.04）。
- 阈值 400,000 ≈ 干净态（7,294）的 55 倍 ⇒ 正常开发约 **63 次 `ys-core` 级构建**才触发一次。
- **先清理、再构建**：单次构建净增 6,247，构建后查到的数字已含本次增量，那时再清等于
  刚建完就拆掉。
- `scripts/par-test.sh` 的构建步骤也改走它（测试构建是同一批 `.o` 的生产者）。

### 6. 仍未做

- §4 的 4 层拆分**未实测**（本机数据显示收益已被清理吃掉，但没做对照测量）。
- `debug = "line-tables-only"`（§未澄清问题 1）仍未决。
- §3 的三个 `#[ignore]` 测试文件裁剪进 `required-features` 仍未做。
