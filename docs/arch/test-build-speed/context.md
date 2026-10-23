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
