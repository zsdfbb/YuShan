//! **基准：同一任务的轮数 / 墙钟 / 耗时切分** —— 测量，不是判定。
//!
//! # 这是基准，不是 e2e 测试
//!
//! 与 `e2e_skills.rs` 同一性质：**只做测量，输出要人看**，`#[ignore]`，**绝不进 CI**
//! （要真凭证、要真金白银、且非确定）。
//!
//! **不做任何内容断言** —— 模型输出非确定，断言内容只会换来 flaky。本文件只打印
//! **形态与耗时**：轮数、墙钟、以及 `ttft` / `gen` / `tools` 的切分。
//!
//! # 为什么必须有它
//!
//! 1. **单次运行没有意义**：同一个提示的轮数方差极大（`tmp/exp` 里开放式提问那次，
//!    四次运行分别是 26 / 31 / 45 / 66 轮）⇒ 必须重复取分布。
//! 2. **「改动前采基线」是 `docs/arch/explore-speed/context.md` 的 Q6**，至今未决 ——
//!    没有它，任何「有没有变快」都是噪声。
//! 3. 耗时的权威读数只有逐轮行（`round=` / `turn=`，见 `src/round_clock.rs`），
//!    手工解析不可复现。
//!
//! # 提示集
//!
//! - `exp/*`：沿用 `tmp/exp` 的单任务与三子任务两条（**轮数基线已存在**，可直接对照）。
//!   同系列的开放式提问 `exp/t4` **已移除** —— 耗时长且无完成标准（见 `PROMPTS` 处注释）。
//! - `daily/*`：四条贴近日常的**只读**任务。
//! - `fact/1`：**可判定**的任务 —— 找出 3 处「文档与代码不一致」，每条给出两个 `文件:行号`
//!   供机械核对，且带**显式工具调用预算**。它是唯一一条**时间与质量同时可测**的提示。
//!
//! ⚠️ **所有提示必须是只读的**。会改工作区的任务在重复运行下自毁可比性 ——
//! 第一次跑完工作区就变了，第二次测的已经不是同一个东西。加提示时请守住这条。
//!
//! # 运行
//!
//! ```text
//! YUSHAN_BENCH_REPEATS=3 \
//!   cargo test -p ys-coding-agent --test e2e_bench -- --ignored --nocapture
//! ```
//!
//! | 环境变量 | 默认 | 作用 |
//! |---|---|---|
//! | `YUSHAN_BENCH_REPEATS` | `1` | 每条提示重复几次 |
//! | `YUSHAN_BENCH_ONLY` | 空 | 只跑名字含该子串的提示（如 `t4`） |
//! | `YUSHAN_BENCH_TIMEOUT_SECS` | `900` | 单次运行的墙钟上限，超时杀掉 |
//! | `YUSHAN_BENCH_HEAVY` | 空 | 连**长跑项**一起跑（默认跳过，见 `PROMPTS`） |
//! | `YUSHAN_BENCH_CLEAN` | 空 | 设了才删临时目录（**默认保留**，见下） |
//!
//! ⚠️ **默认保留证据**：临时目录里有每次运行的**逐轮日志**（`logs/yushan.log`）、
//! 会话转录与 stdout/stderr。跑一次基准要真金白银，删掉就没法回溯「那些轮是怎么长出来的」——
//! 本文件早期版本默认删除，结果一次 246 轮的运行只剩一个中位数。要清理就显式设
//! `YUSHAN_BENCH_CLEAN=1`。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// 提示集：`(名字, 任务文本, 是否长跑)`。
///
/// 见模块头「提示集」—— **只读**是硬约束。
///
/// **长跑项默认不跑**：本文件被两次提醒过这件事 —— 一个开放式提问实测 291–324 秒，
/// 一个"有界"任务实测 348.7 秒。标了 `true` 的项只有 `YUSHAN_BENCH_ONLY` 点名或
/// `YUSHAN_BENCH_HEAVY=1` 才跑，跳过时打印一行提示（不静默）。
/// **当前没有条目用它** —— 机制留着，是因为"新加的提示其实是长跑"已经发生过两次。
///
/// ⚠️ **「数量固定」不是停止条件**。`fact/1`（"找出恰好 3 处不一致"）曾按「有界任务」设计，
/// 实测 **348.7 秒 / 291 轮**，比它要替代的项还长 —— 模型把"3 处"读成"**最好的** 3 处"，
/// 那需要无界搜索。加**显式预算**（"最多 15 次工具调用"）后实测 **114 秒 / 11 轮**，
/// 且三处报告逐条核实**全部为真** ⇒ **预算才是停止条件，数量不是**。
const PROMPTS: &[(&str, &str, bool)] = &[
    (
        "exp/t1",
        "统计 crates/ 下每个 crate 的 src 目录里的 .rs 文件数和总行数，输出一张表。",
        false,
    ),
    (
        "exp/t2",
        "给出仓库 workspace 里有几个 crate；列出 docs/architecture.md 的一级标题；\
         统计 crates/ys-core/src 下所有 .rs 文件的总行数。三个结果一起给我。",
        false,
    ),
    // `exp/t4`（"这个仓库接下来该补什么？"）**已移除** —— 实测 291–324 秒、轮数在 33–246
    // 之间摆动，且**没有可判定的完成标准**（开放式提问，答案无法核对）。它的历史基线
    // 记在 `docs/arch/explore-speed/cost-model.md` §7，需要时按那里复现，不进基准。
    (
        "daily/1",
        "读 crates/ys-event/src/lib.rs，用三句话说明它对外导出什么。",
        false,
    ),
    (
        "daily/2",
        "在仓库里找出 `is_concurrency_safe` 的所有实现与调用点，列出文件与行号。",
        false,
    ),
    (
        "daily/3",
        "读 docs/CONTEXT.md，解释「回合（Turn）」与「轮（Round）」的区别。",
        false,
    ),
    (
        "daily/4",
        "找出 apps/coding-agent/src 下所有模块文件，按职责各一句话归类。",
        false,
    ),
    (
        "fact/1",
        "在仓库里找出 **3 处**「文档声称的状态与代码实际状态不一致」。每处给出：\
         (a) 文档的 `文件:行号`，(b) 代码的 `文件:行号`，(c) 一句话说明为什么不一致。\
         三处必须来自**不同文档**。**只报你亲眼核对过的。**\
         **最多用 15 次工具调用，到第 15 次就必须交卷** —— 少报可以，超预算不行。",
        false,
    ),
];

/// 一次运行的读数。
#[derive(Debug, Clone, Copy, Default)]
struct Reading {
    /// 墙钟（含进程启动）—— 取测试侧 `Instant`，比日志里**整数秒**的 `elapsed` 细。
    wall_ms: u128,
    rounds: u32,
    /// 三段耗时之和（日志里逐轮 `round=` 行累加）。
    ttft_ms: u128,
    gen_ms: u128,
    tools_ms: u128,
}

impl Reading {
    /// 三段各自的占比（以三段之和为分母，不含组装/落盘/转发等余额）。
    fn split(&self) -> (u128, u128, u128) {
        let total = self.ttft_ms + self.gen_ms + self.tools_ms;
        if total == 0 {
            return (0, 0, 0);
        }
        (
            100 * self.ttft_ms / total,
            100 * self.gen_ms / total,
            100 * self.tools_ms / total,
        )
    }
}

/// 取 `key=<数字>` 的值；键不存在或后面不是数字时返回 `None`。
fn field(line: &str, key: &str) -> Option<u128> {
    let start = line.find(key)? + key.len();
    let rest = &line[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    rest[..end].parse().ok()
}

/// 解析一次运行的日志文本 → `(轮数, ttft 合计, gen 合计, tools 合计)`。
///
/// **读到第一行 `turn=` 就停。** 那是**本运行自己的回合终局行**，在它之后写进同一个文件的
/// 一切都不属于这次运行 —— 因为 `YUSHAN_LOG_DIR` 只隔离**目录、不隔离进程**：
/// 模型若自己起一个 `ys-coding-agent`（实测发生过：一次 291 轮的长跑里它自调用了 5 次，
/// 会话目录里留下 `hi` / `write the file` 这些标题），子进程会继承同一个环境变量，
/// 事件全灌进这份日志。不截断的话轮数会虚高数倍、读数直接不可用。
fn parse_log_text(text: &str) -> (u32, u128, u128, u128) {
    let (mut rounds, mut ttft, mut gen_ms, mut tools) = (0u32, 0u128, 0u128, 0u128);
    for line in text.lines() {
        // 本运行的终局行 —— 到此为止（它之后的都是别人写的）。
        if line.contains(" turn=") {
            break;
        }
        // 只认逐轮行：`turn=` 行里也有 `rounds=`，靠 `ttft_ms=` 区分。
        let Some(ttft_one) = field(line, "ttft_ms=") else {
            continue;
        };
        rounds += 1;
        ttft += ttft_one;
        gen_ms += field(line, "gen_ms=").unwrap_or(0);
        tools += field(line, "tools_ms=").unwrap_or(0);
    }
    (rounds, ttft, gen_ms, tools)
}

/// 读日志文件并解析；读不到时全 0（**如实报空，不编数**）。
fn parse_log(path: &Path) -> (u32, u128, u128, u128) {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_log_text(&text),
        Err(_) => (0, 0, 0, 0),
    }
}

/// 跑一次真实二进制，返回读数；超时或异常退出时返回 `None`（**如实报告，不编数**）。
fn run_once(
    bin: &Path,
    workspace: &Path,
    run_dir: &Path,
    task: &str,
    timeout: Duration,
) -> Option<Reading> {
    let log_dir = run_dir.join("logs");
    let sessions = run_dir.join("sessions");
    std::fs::create_dir_all(&log_dir).ok()?;
    std::fs::create_dir_all(&sessions).ok()?;

    let started = Instant::now();
    let mut child = Command::new(bin)
        .arg("-p")
        .arg(task)
        .current_dir(workspace)
        // 日志与会话各自隔离到临时目录；**凭证仍走真实 HOME**（持久化 auth），
        // 故不改 HOME —— 这正是 `YUSHAN_LOG_DIR` 存在的理由。
        .env("YUSHAN_LOG_DIR", &log_dir)
        .env("YUSHAN_SESSIONS_DIR", &sessions)
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::fs::File::create(run_dir.join("stdout.txt")).ok()?))
        .stderr(Stdio::from(std::fs::File::create(run_dir.join("stderr.txt")).ok()?))
        .spawn()
        .ok()?;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    eprintln!("  ! 退出码 {status}（见 {}/stderr.txt）", run_dir.display());
                }
                break;
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("  ! 等待子进程失败：{e}");
                return None;
            }
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("  ! 超时 {}s，已杀", timeout.as_secs());
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    let (rounds, ttft_ms, gen_ms, tools_ms) = parse_log(&log_dir.join("yushan.log"));
    Some(Reading {
        wall_ms: started.elapsed().as_millis(),
        rounds,
        ttft_ms,
        gen_ms,
        tools_ms,
    })
}

/// 临时目录（进程内原子序号 + pid —— 与全仓测试隔离约定一致）。
fn temp_dir(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ys-bench-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

fn sanitize(name: &str) -> String {
    name.replace('/', "_")
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn median(xs: &[u128]) -> u128 {
    if xs.is_empty() {
        return 0;
    }
    let mut v = xs.to_vec();
    v.sort_unstable();
    v[v.len() / 2]
}

fn secs(ms: u128) -> String {
    format!("{:.1}", ms as f64 / 1000.0)
}

/// 只有一条 `#[test]`：跑完整组提示，逐条打印，最后打印中位数汇总。
///
/// **不断言任何东西** —— 见模块头。它失败的唯一方式是环境缺凭证 / 子进程起不来。
#[test]
#[ignore = "基准：需真实凭证 + 真金白银，手动跑（见模块头）"]
fn bench_report() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_ys-coding-agent"));
    // 工作区固定为仓库根：跨运行可比的前提是「同一个工作区、只读任务」。
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("仓库根");
    let repeats = env_usize("YUSHAN_BENCH_REPEATS", 1).max(1);
    let timeout = Duration::from_secs(env_usize("YUSHAN_BENCH_TIMEOUT_SECS", 900) as u64);
    let only = std::env::var("YUSHAN_BENCH_ONLY").unwrap_or_default();
    // **默认保留**：证据（逐轮日志 / 转录 / stdout）比磁盘金贵，见模块头。
    let clean = std::env::var("YUSHAN_BENCH_CLEAN").is_ok();
    // 长跑项默认不跑（见 `PROMPTS` 的说明）；点名或显式开关才跑。
    let heavy_enabled = std::env::var("YUSHAN_BENCH_HEAVY").is_ok();
    let root = temp_dir("bench");

    println!(
        "\n基准：{} 条提示 × {repeats} 次   工作区 {}\n临时目录 {}\n",
        PROMPTS.len(),
        workspace.display(),
        root.display()
    );

    // (名字, 墙钟中位, 轮中位, ttft 中位, gen 中位, tools 中位)
    let mut summary: Vec<(String, u128, u128, u128, u128, u128)> = Vec::new();
    let mut skipped_heavy: Vec<&str> = Vec::new();
    for (name, task, heavy) in PROMPTS {
        // 点名优先：`YUSHAN_BENCH_ONLY` 命中的项**即使标了长跑也跑**（那是显式要求）。
        let named = !only.is_empty() && name.contains(only.as_str());
        if !only.is_empty() && !named {
            continue;
        }
        if *heavy && !named && !heavy_enabled {
            skipped_heavy.push(name);
            continue;
        }
        println!("=== {name} ===");
        let mut readings = Vec::new();
        for i in 0..repeats {
            let run_dir = root.join(format!("{}-{}", sanitize(name), i));
            std::fs::create_dir_all(&run_dir).expect("建运行目录");
            match run_once(&bin, &workspace, &run_dir, task, timeout) {
                Some(r) => {
                    let (t, g, s) = r.split();
                    println!(
                        "  run {}  墙钟 {}s  轮 {:<3} ttft {}%  gen {}%  tools {}%   ({}/{}/{} ms)",
                        i + 1,
                        secs(r.wall_ms),
                        r.rounds,
                        t,
                        g,
                        s,
                        r.ttft_ms,
                        r.gen_ms,
                        r.tools_ms,
                    );
                    readings.push(r);
                }
                None => println!("  run {}  无读数（见上方错误）", i + 1),
            }
        }
        if readings.is_empty() {
            println!("  （本条无任何读数）\n");
            continue;
        }
        let walls: Vec<u128> = readings.iter().map(|r| r.wall_ms).collect();
        let rounds: Vec<u128> = readings.iter().map(|r| r.rounds as u128).collect();
        let ttfts: Vec<u128> = readings.iter().map(|r| r.ttft_ms).collect();
        let gens: Vec<u128> = readings.iter().map(|r| r.gen_ms).collect();
        let tools: Vec<u128> = readings.iter().map(|r| r.tools_ms).collect();
        println!(
            "  **中位**  墙钟 {}s  轮 {}  ({}/{}/{} ms)\n",
            secs(median(&walls)),
            median(&rounds),
            median(&ttfts),
            median(&gens),
            median(&tools),
        );
        summary.push((
            (*name).to_string(),
            median(&walls),
            median(&rounds),
            median(&ttfts),
            median(&gens),
            median(&tools),
        ));
    }

    println!("=== 汇总（中位）===");
    println!("{:<10} {:>8} {:>5} {:>10} {:>10} {:>10}", "提示", "墙钟s", "轮", "ttft_ms", "gen_ms", "tools_ms");
    for (name, wall, rounds, ttft, gen_ms, tools) in &summary {
        println!(
            "{name:<10} {:>8} {rounds:>5} {ttft:>10} {gen_ms:>10} {tools:>10}",
            secs(*wall)
        );
    }
    println!();
    if !skipped_heavy.is_empty() {
        println!(
            "跳过的长跑项（默认不跑，要跑用 YUSHAN_BENCH_ONLY=<名> 点名或 YUSHAN_BENCH_HEAVY=1）：{}",
            skipped_heavy.join(", ")
        );
        println!();
    }

    if clean {
        let _ = std::fs::remove_dir_all(&root);
    } else {
        println!("证据保留在：{}", root.display());
        println!("  逐轮读数：{}/<提示>-<序号>/logs/yushan.log", root.display());
        println!("  想删掉：设 YUSHAN_BENCH_CLEAN=1 重跑，或直接 rm -rf\n");
    }
}

/// `parse_log_text` 必须在**本运行的终局行**处截断。
///
/// 背景：模型可能自己起一个 `ys-coding-agent`（实测发生过 —— 一次 291 轮的长跑里它自调用了
/// 5 次），子进程继承同一个 `YUSHAN_LOG_DIR`，其事件灌进同一份日志。不截断则轮数虚高、
/// 读数不可用。
///
/// 变异：把 `if line.contains(" turn=") { break; }` 短路掉 ⇒ 第一条断言从 2 变成 4
/// （已实际注入验证）。
#[test]
fn parse_log_stops_at_the_first_turn_line() {
    let text = "\
[1] round=1 ttft_ms=10 gen_ms=20 tools_ms=30 total_ms=60\n\
[2] round=2 ttft_ms=11 gen_ms=21 tools_ms=31 total_ms=63\n\
[3] turn=1 rounds=2 elapsed=5s cumulative_rounds=2 budget_remaining=1s cache_hit=0 cache_miss=0\n\
[4] round=1 ttft_ms=999 gen_ms=999 tools_ms=999 total_ms=999\n\
[5] round=2 ttft_ms=999 gen_ms=999 tools_ms=999 total_ms=999\n";
    let (rounds, ttft, gen_ms, tools) = parse_log_text(text);
    assert_eq!(rounds, 2, "只数本运行的轮；子进程写进来的必须忽略");
    assert_eq!((ttft, gen_ms, tools), (21, 41, 61));

    // 没有终局行（崩溃等）⇒ 全量计入，不丢数据。
    let no_turn = "[1] round=1 ttft_ms=5 gen_ms=6 tools_ms=7 total_ms=18\n";
    assert_eq!(parse_log_text(no_turn), (1, 5, 6, 7));
}
