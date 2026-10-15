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
//! - `exp/*`：沿用 `tmp/exp` 那三条（单任务 / 三子任务 / 开放式提问）——
//!   **后两条的轮数基线已存在**，可直接对照。
//! - `daily/*`：四条贴近日常的**只读**任务。
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

/// 提示集：`(名字, 任务文本)`。见模块头「提示集」—— **只读**是硬约束。
const PROMPTS: &[(&str, &str)] = &[
    (
        "exp/t1",
        "统计 crates/ 下每个 crate 的 src 目录里的 .rs 文件数和总行数，输出一张表。",
    ),
    (
        "exp/t2",
        "给出仓库 workspace 里有几个 crate；列出 docs/architecture.md 的一级标题；\
         统计 crates/ys-core/src 下所有 .rs 文件的总行数。三个结果一起给我。",
    ),
    (
        "exp/t4",
        "这个仓库接下来该补什么？请基于代码与文档的实际状态给出建议。",
    ),
    (
        "daily/1",
        "读 crates/ys-event/src/lib.rs，用三句话说明它对外导出什么。",
    ),
    (
        "daily/2",
        "在仓库里找出 `is_concurrency_safe` 的所有实现与调用点，列出文件与行号。",
    ),
    (
        "daily/3",
        "读 docs/CONTEXT.md，解释「回合（Turn）」与「轮（Round）」的区别。",
    ),
    (
        "daily/4",
        "找出 apps/coding-agent/src 下所有模块文件，按职责各一句话归类。",
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

/// 解析一次运行的日志 → `(轮数, ttft 合计, gen 合计, tools 合计)`。
///
/// 每次运行独占一个日志目录（见 [`run_once`]），故这里不担心跨运行累积。
fn parse_log(path: &Path) -> (u32, u128, u128, u128) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (0, 0, 0, 0);
    };
    let (mut rounds, mut ttft, mut gen_ms, mut tools) = (0u32, 0u128, 0u128, 0u128);
    for line in text.lines() {
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
    let root = temp_dir("bench");

    println!(
        "\n基准：{} 条提示 × {repeats} 次   工作区 {}\n临时目录 {}\n",
        PROMPTS.len(),
        workspace.display(),
        root.display()
    );

    // (名字, 墙钟中位, 轮中位, ttft 中位, gen 中位, tools 中位)
    let mut summary: Vec<(String, u128, u128, u128, u128, u128)> = Vec::new();
    for (name, task) in PROMPTS {
        if !only.is_empty() && !name.contains(only.as_str()) {
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

    if clean {
        let _ = std::fs::remove_dir_all(&root);
    } else {
        println!("证据保留在：{}", root.display());
        println!("  逐轮读数：{}/<提示>-<序号>/logs/yushan.log", root.display());
        println!("  想删掉：设 YUSHAN_BENCH_CLEAN=1 重跑，或直接 rm -rf\n");
    }
}
