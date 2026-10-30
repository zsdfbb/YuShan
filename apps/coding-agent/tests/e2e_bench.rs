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
//! 3. 每次运行带 `--debug`：优先解析本进程的 debug JSONL（逐轮分段、HTTP 尝试、
//!    退避、缓存），文本日志只作回退。手工解析不可复现。
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
//! | `YUSHAN_BENCH_PROTOCOL` | `chat` | 协议面档位：`chat` / `responses`（非法值回退 `chat`） |
//! | `YUSHAN_BENCH_BASE` | 空 | 覆盖子进程的 `YUSHAN_API_BASE`（协议面 A/B 用） |
//! | `YUSHAN_BENCH_OUT` | 空 | 汇总 JSON 的固定落盘路径；已存在时**追加**到数组。未设置 ⇒ 写本次 run 目录 |
//!
//! 每条结果（提示 × repeat）另落一条 JSON 记录到 `bench-summary.json`，供跨轮对比；
//! **stdout 表格是字节稳定的契约**，新增读数只进 JSON，不改表格。
//!
//! ⚠️ **默认保留证据**：临时目录里有每次运行的**逐轮日志**（`logs/yushan.log`）、
//! 会话转录与 stdout/stderr。跑一次基准要真金白银，删掉就没法回溯「那些轮是怎么长出来的」——
//! 本文件早期版本默认删除，结果一次 246 轮的运行只剩一个中位数。要清理就显式设
//! `YUSHAN_BENCH_CLEAN=1`。
//!
//! # 协议面（`YUSHAN_BENCH_PROTOCOL`）
//!
//! 档位只有两个：`chat`（OpenAI 兼容的 `/chat/completions`）与 `responses`
//! （Responses API）。**应用侧只实现了 `chat`** —— `responses` 档由**外部端点 / shim**
//! 提供：shim 接受 `POST <base>/chat/completions`，把请求转发到 Responses API。
//! 本仓库不给 `ys-model` 加 Responses 后端（那要动 `Model` trait 与流式事件形态）。
//!
//! 档位与 base **各管一件事**：档位说「对面讲的是哪种协议」（记录表里的「协议」列），
//! base 说「跟谁说话」（`YUSHAN_BENCH_BASE` 覆盖子进程的 `YUSHAN_API_BASE`；未设置
//! 则沿用继承的 `YUSHAN_API_BASE`）。一次 A/B = 两次基准运行，两次之间只换这两个变量。
//!
//! ⚠️ **base 覆盖要配合 env 凭证**：应用侧的模型解析是「env 完整凭证优先」
//! （`apps/coding-agent/src/main.rs:1441` 档 1）—— `YUSHAN_API_BASE` 与
//! `YUSHAN_API_KEY` **都在**才用 env 的 base；只有 base 没有 key 时，会退回
//! `/login` 存下的 provider base ⇒ 两次 A/B 打在同一个端点上。基准会在这种情况下
//! 打一行提示。
//!
//! # 质量位
//!
//! **不可只看墙钟** —— 质量必须同时记录。本文件只做**机械**核对（模型输出非确定，
//! 语义判定不可复现，断言它只会换来 flaky）：
//!
//! - 答案非空；
//! - 证据引用（`文件:行号`）够不够（逐提示的判据见 `QUALITY_SPECS`）；
//! - 引用是否指向仓库里**真实存在**的文件与行 —— 这是「结论是否与证据一致」的机械代理。
//!
//! 取值：`Q+` 全过 / `Q~` 部分过 / `Q-` 不过 / `Q?` 该提示未设机械判据。

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

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
        "sweep/1",
        "给 `docs/arch/` 下**每一个**二级目录各写一句话说明它在讲什么——依据目录里的文档，\
         不要猜。按目录名排序，输出一张两列表：`目录名 | 一句话`。",
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
    /// HTTP 尝试数；含内部重试与 `stream_options` 回退后的请求。
    attempts: u128,
    /// 实际进入退避的重试次数与累计等待。
    retried_attempts: u128,
    retry_wait_ms: u128,
    cache_hit: u128,
    cache_miss: u128,
    /// 质量位（机械核对的结果；跑完拿到答案后才回填，见 `quality_of`）。
    quality: Quality,
    /// 每轮工具调用数 ×100（整数口径，避免浮点）。取自 debug 的 `run_reading`。
    tpr_x100: u128,
    /// 每轮输入 token 中位数。取自 debug 的 `run_reading`。
    input_med: u128,
    /// 只有 1 个工具调用的轮数。取自 debug 的 `run_reading`；记录没带该字段时
    /// 由 `round_end.tool_calls == 1` 的轮数兜底。
    single_call_rounds: u32,
    /// 是否读到 `run_reading` 记录。为假时上面三个字段在 JSON 里写 `null` ——
    /// 「没有读数」不得伪装成 0。
    run_reading_seen: bool,
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

    /// 一条结果的 JSON 形态。
    ///
    /// `run_reading` 缺失时三个新字段写 `null`，与 stdout 表格里的「-」同义；
    /// 其余字段沿用表格口径（整数毫秒 / 计数 / 质量位短标签）。
    fn to_json(self) -> Value {
        let reading_field = |value: u128| {
            if self.run_reading_seen {
                u128_json(value)
            } else {
                Value::Null
            }
        };
        json!({
            "wall_ms": u128_json(self.wall_ms),
            "rounds": self.rounds,
            "ttft_ms": u128_json(self.ttft_ms),
            "gen_ms": u128_json(self.gen_ms),
            "tools_ms": u128_json(self.tools_ms),
            "attempts": u128_json(self.attempts),
            "retried_attempts": u128_json(self.retried_attempts),
            "retry_wait_ms": u128_json(self.retry_wait_ms),
            "cache_hit": u128_json(self.cache_hit),
            "cache_miss": u128_json(self.cache_miss),
            "cache_pct": cache_pct(self.cache_hit, self.cache_miss),
            "quality": self.quality.label(),
            "tpr_x100": reading_field(self.tpr_x100),
            "input_med": reading_field(self.input_med),
            "single_call_rounds": reading_field(u128::from(self.single_call_rounds)),
        })
    }

    /// 吸收一条 `run_reading` debug 记录。
    ///
    /// 只要记录存在就置 `run_reading_seen`（「记录在、字段缺」与「记录不在」
    /// 在 JSON 里要能区分）。`single_call_rounds` 缺失时用 `derived` 兜底 ——
    /// 该值由同一份 debug 里的 `round_end.tool_calls` 数出来，不额外假设 schema。
    fn absorb_run_reading(&mut self, value: &Value, derived: u32) {
        self.run_reading_seen = true;
        // 生产记录同时带 `line` 与顶层数字字段。顶层缺失时从 `line` 反解 ——
        // 「有记录却读成假 0」比「没有记录」更具误导性，故必须有这条兜底。
        let line = value.get("line").and_then(Value::as_str);
        let read = |key: &str| {
            json_u128(value, key).or_else(|| line.and_then(|line| field(line, &format!("{key}="))))
        };
        self.tpr_x100 = read("tpr_x100").unwrap_or(0);
        self.input_med = read("input_med").unwrap_or(0);
        self.single_call_rounds = read("single_call_rounds")
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(derived);
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

fn json_u128(value: &Value, key: &str) -> Option<u128> {
    value.get(key)?.as_u64().map(u128::from)
}

/// `u128` → JSON 数字。基准读数远小于 `u64::MAX`；超出时饱和（不 panic）。
fn u128_json(value: u128) -> Value {
    Value::from(u64::try_from(value).unwrap_or(u64::MAX))
}

/// 解析本次运行的 debug JSONL。
///
/// 以第一条 `run_end` 为界：同一会话文件可能含多次 `--resume` 运行，基准的一次
/// 子进程只对应第一个 run。`model_attempt` 与 `round_usage` 提供文本日志没有的
/// 重试/退避/缓存读数。
///
/// `run_reading` 是**可选**记录（A1 的出口；落盘顺序不保证在 `run_end` 之前）：
/// 在第一个 `run_end` 之前或之后读到都吸收；遇到下一次 `run_start` 即停止。
/// 完全没有该记录时三个新字段保持 0 且 `run_reading_seen = false`，不 panic。
fn parse_debug_text(text: &str) -> Option<Reading> {
    let mut reading = Reading::default();
    let mut saw_round = false;
    let mut finished = false;
    // `run_reading` 没带 `single_call_rounds` 时的兜底：同一份 debug 里
    // `round_end.tool_calls == 1` 的轮数。
    let mut derived_single_call_rounds = 0u32;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ev = value.get("ev").and_then(Value::as_str);
        if finished {
            match ev {
                // 下一次 `--resume` 的 run 从这里开始：本 run 的读数到此为止。
                Some("run_start") => break,
                Some("run_reading") => {
                    reading.absorb_run_reading(&value, derived_single_call_rounds)
                }
                _ => {}
            }
            continue;
        }
        match ev {
            Some("round_end") => {
                saw_round = true;
                reading.rounds = reading.rounds.saturating_add(1);
                reading.ttft_ms = reading
                    .ttft_ms
                    .saturating_add(json_u128(&value, "ttft_ms").unwrap_or(0));
                reading.gen_ms = reading
                    .gen_ms
                    .saturating_add(json_u128(&value, "gen_ms").unwrap_or(0));
                reading.tools_ms = reading
                    .tools_ms
                    .saturating_add(json_u128(&value, "tools_ms").unwrap_or(0));
                if json_u128(&value, "tool_calls") == Some(1) {
                    derived_single_call_rounds = derived_single_call_rounds.saturating_add(1);
                }
            }
            Some("model_attempt") => {
                reading.attempts = reading.attempts.saturating_add(1);
                if value.get("retried").and_then(Value::as_bool) == Some(true) {
                    reading.retried_attempts = reading.retried_attempts.saturating_add(1);
                    reading.retry_wait_ms = reading
                        .retry_wait_ms
                        .saturating_add(json_u128(&value, "backoff_ms").unwrap_or(0));
                }
            }
            Some("round_usage") if value.get("purpose").and_then(Value::as_str) == Some("turn") => {
                let input = json_u128(&value, "gen_ai.usage.input_tokens").unwrap_or(0);
                let hit = json_u128(&value, "gen_ai.usage.cache_read.input_tokens").unwrap_or(0);
                reading.cache_hit = reading.cache_hit.saturating_add(hit);
                reading.cache_miss = reading.cache_miss.saturating_add(input.saturating_sub(hit));
            }
            Some("run_reading") => reading.absorb_run_reading(&value, derived_single_call_rounds),
            Some("run_end") => finished = true,
            _ => {}
        }
    }
    saw_round.then_some(reading)
}

/// 按父进程 PID 选 debug 文件。模型可能自起 `ys-coding-agent` 子进程，它们共享
/// `YUSHAN_LOG_DIR`；只按目录里第一个文件读会把两棵运行树混在一起。
fn debug_file_for_pid(log_dir: &Path, pid: u32) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(log_dir).ok()?.filter_map(Result::ok) {
        let path = entry.path();
        let is_debug = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("debug-") && name.ends_with(".jsonl"));
        if !is_debug {
            continue;
        }
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let owner = BufReader::new(file).lines().take(64).find_map(|line| {
            let line = line.ok()?;
            let value: Value = serde_json::from_str(&line).ok()?;
            (value.get("ev").and_then(Value::as_str) == Some("run_start"))
                .then(|| value.get("pid").and_then(Value::as_u64))
                .flatten()
        });
        match owner {
            Some(owner) if owner == u64::from(pid) => return Some(path),
            None => candidates.push(path),
            _ => {}
        }
    }
    // 老 schema / 截断文件没有 `pid` 时，只有一个候选才敢用。
    (candidates.len() == 1).then(|| candidates.remove(0))
}

fn parse_debug(path: &Path, pid: u32) -> Option<Reading> {
    let path = debug_file_for_pid(path, pid)?;
    let text = std::fs::read_to_string(path).ok()?;
    parse_debug_text(&text)
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
        .arg("--debug")
        .arg(task)
        .current_dir(workspace)
        // 日志与会话各自隔离到临时目录；**凭证仍走真实 HOME**（持久化 auth），
        // 故不改 HOME —— 这正是 `YUSHAN_LOG_DIR` 存在的理由。
        .env("YUSHAN_LOG_DIR", &log_dir)
        .env("YUSHAN_SESSIONS_DIR", &sessions)
        // base 覆盖（协议面 A/B）：`YUSHAN_BENCH_BASE` 未设置 ⇒ 不动继承的 base。
        .envs(base_override())
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            std::fs::File::create(run_dir.join("stdout.txt")).ok()?,
        ))
        .stderr(Stdio::from(
            std::fs::File::create(run_dir.join("stderr.txt")).ok()?,
        ))
        .spawn()
        .ok()?;
    let pid = child.id();

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

    let mut reading = parse_debug(&log_dir, pid).unwrap_or_else(|| {
        let (rounds, ttft_ms, gen_ms, tools_ms) = parse_log(&log_dir.join("yushan.log"));
        Reading {
            rounds,
            ttft_ms,
            gen_ms,
            tools_ms,
            ..Reading::default()
        }
    });
    reading.wall_ms = started.elapsed().as_millis();
    Some(reading)
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

fn cache_pct(hit: u128, miss: u128) -> String {
    let total = hit + miss;
    (100 * hit)
        .checked_div(total)
        .map(|pct| format!("{pct}%"))
        .unwrap_or_else(|| "-".to_string())
}

// ─────────────────────────────── 汇总落盘 ───────────────────────────────

/// 汇总 JSON 的固定落盘路径开关。
const BENCH_OUT_ENV: &str = "YUSHAN_BENCH_OUT";

/// 汇总 JSON 的落盘路径。
///
/// `YUSHAN_BENCH_OUT` 存在且非空 ⇒ 用它（固定路径，跨运行追加）；否则写本次
/// run 目录。空串视同未设置，与仓库其他 env 的口径一致。
fn bench_summary_path(run_root: &Path) -> PathBuf {
    match std::env::var(BENCH_OUT_ENV) {
        Ok(path) if !path.trim().is_empty() => PathBuf::from(path),
        _ => run_root.join("bench-summary.json"),
    }
}

/// 一条落盘记录：读数 + 定位信息（提示名 / 第几次 repeat / 协议面）。
fn bench_record(name: &str, repeat: usize, protocol: Protocol, reading: &Reading) -> Value {
    let mut record = reading.to_json();
    if let Value::Object(fields) = &mut record {
        fields.insert("name".to_string(), json!(name));
        fields.insert("repeat".to_string(), json!(repeat));
        fields.insert("protocol".to_string(), json!(protocol.label()));
    }
    record
}

/// 追加一条记录到 JSON 数组。
///
/// 语义：文件不存在 / 空 ⇒ 新建数组；已是数组 ⇒ 追加。选「追加」而非「覆盖」，
/// 是为了 `YUSHAN_BENCH_OUT` 指向固定路径时能跨运行对比。
///
/// 文件存在但内容损坏（非法 JSON / 顶层不是数组）时**先改名备份再写**，
/// 绝不把历史基准记录当空数组静默清空；备份也失败则跳过本次写入。
/// 落盘失败只告警：读数已经打印在 stdout，基准本身不该因此失败。
fn append_json_record(path: &Path, record: Value) {
    if let Err(error) = try_append_json_record(path, record) {
        eprintln!("  ! 写 {} 失败：{error}", path.display());
    }
}

fn try_append_json_record(path: &Path, record: Value) -> std::io::Result<()> {
    let mut records = match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Array(items)) => items,
            // 读成功但内容不是数组 / 不是合法 JSON：**绝不当作空数组覆盖** ——
            // 那是付费基准的历史记录。先改名备份，备份失败就跳过本次写入。
            _ => {
                let backup = corrupt_backup_path(path);
                if let Err(error) = std::fs::rename(path, &backup) {
                    return Err(std::io::Error::new(
                        error.kind(),
                        format!(
                            "{} 不是合法 JSON 数组，备份到 {} 失败，跳过本次写入：{error}",
                            path.display(),
                            backup.display()
                        ),
                    ));
                }
                eprintln!(
                    "  ! {} 不是合法 JSON 数组，已备份到 {}",
                    path.display(),
                    backup.display()
                );
                Vec::new()
            }
        },
        // 文件不存在（或读不到）⇒ 新建数组，与既有语义一致。
        Err(_) => Vec::new(),
    };
    records.push(record);
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut text =
        serde_json::to_string_pretty(&Value::Array(records)).map_err(std::io::Error::other)?;
    text.push('\n');
    std::fs::write(path, text)
}

/// 损坏文件的备份路径：`<name>.corrupt-<pid>-<进程内序号>`。
///
/// 序号用原子计数 + `process::id()`，与全仓测试隔离约定一致。
fn corrupt_backup_path(path: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("bench-summary.json");
    path.with_file_name(format!(
        "{name}.corrupt-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

// ─────────────────────────────── 协议面 ───────────────────────────────

/// 协议面档位。
///
/// `Chat` = OpenAI 兼容的 `/chat/completions`（应用唯一实现的协议面）；
/// `Responses` = Responses API —— **应用侧不实现**该后端，由外部端点 / shim 提供
/// （见模块头「协议面」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    Chat,
    Responses,
}

impl Protocol {
    /// 表格与日志里的标签。
    fn label(self) -> &'static str {
        match self {
            Protocol::Chat => "chat",
            Protocol::Responses => "responses",
        }
    }
}

/// 协议面开关的环境变量名。
const PROTOCOL_ENV: &str = "YUSHAN_BENCH_PROTOCOL";

/// base 覆盖的环境变量名（覆盖子进程的 `YUSHAN_API_BASE`）。
const BASE_ENV: &str = "YUSHAN_BENCH_BASE";

/// 解析协议面取值：**未设置或非法值 ⇒ `chat`**（不 panic）。
///
/// 基准不该因一个拼错的开关整个挂掉：拼错时回退默认档，实际档位照实记进记录表。
fn parse_protocol(raw: Option<&str>) -> Protocol {
    let normalized = raw.map(str::trim).map(str::to_ascii_lowercase);
    match normalized.as_deref() {
        Some("responses") => Protocol::Responses,
        _ => Protocol::Chat,
    }
}

/// 读环境里的协议面档位。
fn protocol_from_env() -> Protocol {
    parse_protocol(std::env::var(PROTOCOL_ENV).ok().as_deref())
}

/// 协议面 A/B 用的 base 覆盖；未设置 ⇒ 子进程沿用继承的 `YUSHAN_API_BASE`。
fn base_override() -> Option<(&'static str, String)> {
    let base = std::env::var(BASE_ENV).ok()?;
    let base = base.trim().to_string();
    (!base.is_empty()).then_some(("YUSHAN_API_BASE", base))
}

/// base 覆盖是否**会真的生效**。
///
/// 应用侧的模型解析是「env 完整凭证优先」（`apps/coding-agent/src/main.rs:1441` 档 1）：
/// base 与 key 都在才用 env 的 base；只有 base 没有 key 时会退回 `/login` 存下的
/// provider base —— 那就不是在测指定的端点。
fn base_override_effective() -> bool {
    base_override().is_some()
        && (std::env::var("YUSHAN_API_KEY").is_ok() || std::env::var("OPENAI_API_KEY").is_ok())
}

// ─────────────────────────────── 质量位 ───────────────────────────────

/// 质量判据：只做**机械**核对，不对结论做语义判定。
struct QualitySpec {
    /// 期望的最少证据引用数（去重后的、指向仓库真实位置的引用）。
    min_citations: usize,
}

/// 有机械判据的提示；其余提示的质量位记「未设判据」。
const QUALITY_SPECS: &[(&str, QualitySpec)] = &[
    // 3 处不一致 × (文档 + 代码) 各一条证据。
    ("fact/1", QualitySpec { min_citations: 6 }),
    // 至少一处实现 + 一处调用点。
    ("daily/2", QualitySpec { min_citations: 2 }),
];

/// 质量位。
///
/// 派生顺序即**严重度**（`Fail` 最大）—— 汇总时取各次运行里的最差值。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
enum Quality {
    /// 该提示没有可机械核对的判据。
    #[default]
    Unjudged,
    /// 判据全过。
    Pass,
    /// 部分过（有引用指向不存在的位置）。
    Partial,
    /// 不过（无输出 / 证据不足）。
    Fail,
}

impl Quality {
    /// 表格里的短标签。
    fn label(self) -> &'static str {
        match self {
            Quality::Unjudged => "Q?",
            Quality::Pass => "Q+",
            Quality::Partial => "Q~",
            Quality::Fail => "Q-",
        }
    }
}

/// 取提示的质量判据；没有则 `None`（质量位记「未设判据」）。
fn quality_spec(name: &str) -> Option<&'static QualitySpec> {
    QUALITY_SPECS
        .iter()
        .find(|(prompt, _)| *prompt == name)
        .map(|(_, spec)| spec)
}

/// 一条证据引用（`文件:行号`）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Citation {
    path: String,
    line: u64,
}

/// 只有以这些顶层目录开头的引用才**可核对** —— 只有它们知道相对仓库根的位置。
///
/// 其余写法（如 `src/lib.rs`）无从判断基准目录，只计数、不判真假。
const ROOTED_DIRS: &[&str] = &["crates", "adapters", "apps", "docs", "scripts", "examples"];

/// 路径字符：ASCII 字母数字与 `_` `-` `.` `/`。
///
/// 刻意**不含**非 ASCII —— 中文正文里紧贴路径（"见crates/x.rs:12"）时，
/// 路径段才能从真正的路径起点开始。
fn is_path_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/')
}

/// 末段是否带扩展名（`.` 后面还有字符）。
fn has_extension(path: &str) -> bool {
    let last = path.rsplit('/').next().unwrap_or(path);
    match last.rfind('.') {
        Some(dot) => dot + 1 < last.len(),
        None => false,
    }
}

/// `:`（或全角 `：`）后紧跟的数字段 ⇒ 行号；返回 (行号, 消耗的字符数)。
fn leading_line(text: &str) -> Option<(u64, usize)> {
    let colon = text.chars().next()?;
    if colon != ':' && colon != '：' {
        return None;
    }
    let tail = &text[colon.len_utf8()..];
    let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let consumed = colon.len_utf8() + digits.len();
    digits.parse().ok().map(|line| (line, consumed))
}

/// 从答案文本里抽出 `文件:行号` 形式的证据引用。
///
/// 路径取「路径字符」的极大连续段，其后紧跟 `:`（或全角 `：`）加数字即成一条引用。
/// 末段必须带扩展名 —— 这样 `localhost:8080` 这类不会混进来。
fn citations(text: &str) -> Vec<Citation> {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < text.len() {
        let Some(ch) = text[offset..].chars().next() else {
            break;
        };
        if !is_path_char(ch) {
            offset += ch.len_utf8();
            continue;
        }
        let start = offset;
        while offset < text.len() {
            let Some(ch) = text[offset..].chars().next() else {
                break;
            };
            if !is_path_char(ch) {
                break;
            }
            offset += ch.len_utf8();
        }
        let path = text[start..offset].trim_end_matches('.');
        let Some((line, consumed)) = leading_line(&text[offset..]) else {
            continue;
        };
        offset += consumed;
        if path.is_empty() || !has_extension(path) {
            continue;
        }
        out.push(Citation {
            path: path.to_string(),
            line,
        });
    }
    out
}

/// 引用是否**可核对**：路径首段是仓库的顶层目录。
fn is_judgeable(citation: &Citation) -> bool {
    let first = citation.path.split('/').next().unwrap_or("");
    ROOTED_DIRS.contains(&first)
}

/// 引用是否指向仓库里**真实存在**的位置。
///
/// 「存在」= 文件在仓库里找得到，且行数不少于引用行号。
fn citation_exists(root: &Path, citation: &Citation) -> bool {
    let Ok(text) = std::fs::read_to_string(root.join(&citation.path)) else {
        return false;
    };
    text.lines().count() as u64 >= citation.line
}

/// 按判据核对一次运行的答案 ⇒ 质量位。
///
/// 规则：
/// - 答案为空 ⇒ 不过；
/// - 有判据：可核对的引用不足 ⇒ 不过；有引用指向不存在的位置 ⇒ 部分过；
/// - 无判据：没有可核对的引用 ⇒ 未设判据；否则按引用是否指向真实位置记通过 / 部分过。
fn quality_of(answer: &str, root: &Path, spec: Option<&QualitySpec>) -> Quality {
    if answer.trim().is_empty() {
        return Quality::Fail;
    }
    let mut judged: Vec<&Citation> = Vec::new();
    let found = citations(answer);
    for citation in &found {
        if is_judgeable(citation) && !judged.contains(&citation) {
            judged.push(citation);
        }
    }
    let invalid = judged
        .iter()
        .filter(|citation| !citation_exists(root, citation))
        .count();
    match spec {
        Some(spec) => {
            if judged.len() < spec.min_citations {
                return Quality::Fail;
            }
            if invalid > 0 {
                Quality::Partial
            } else {
                Quality::Pass
            }
        }
        None => {
            if judged.is_empty() {
                Quality::Unjudged
            } else if invalid > 0 {
                Quality::Partial
            } else {
                Quality::Pass
            }
        }
    }
}

/// 读一次运行的答案文本（`-p` 模式的 stdout —— 只有模型的文本增量）。
fn answer_text(run_dir: &Path) -> String {
    std::fs::read_to_string(run_dir.join("stdout.txt")).unwrap_or_default()
}

/// 汇总表的一行：`(名字, 墙钟, 轮, ttft, gen, tools, attempts, retries, retry_wait, cache_hit, cache_miss, 质量)`。
type Summary = (
    String,
    u128,
    u128,
    u128,
    u128,
    u128,
    u128,
    u128,
    u128,
    u128,
    u128,
    Quality,
);

/// 汇总表的表头（列名）。
fn summary_header() -> String {
    format!(
        "{:<10} {:<9} {:>8} {:>5} {:>10} {:>10} {:>10} {:>4} {:>9} {:>8} {:>12} {:>7}",
        "提示",
        "协议",
        "墙钟s",
        "轮",
        "ttft_ms",
        "gen_ms",
        "tools_ms",
        "质量",
        "attempts",
        "retries",
        "retry_wait",
        "cache"
    )
}

/// 汇总表的一行。
fn summary_row(row: &Summary, protocol: Protocol) -> String {
    let (
        name,
        wall,
        rounds,
        ttft,
        gen_ms,
        tools,
        attempts,
        retries,
        retry_wait,
        hit,
        miss,
        quality,
    ) = row;
    format!(
        "{name:<10} {proto_label:<9} {:>8} {rounds:>5} {ttft:>10} {gen_ms:>10} {tools:>10} {qual_label:>4} {attempts:>9} {retries:>8} {retry_wait:>12} {:>7}",
        secs(*wall),
        cache_pct(*hit, *miss),
        proto_label = protocol.label(),
        qual_label = quality.label(),
    )
}

/// 只有一条 `#[test]`：跑完整组提示，逐条打印，最后打印中位数汇总。
///
/// **不断言任何东西** —— 见模块头。它失败的唯一方式是环境缺凭证 / 子进程起不来。
#[test]
#[ignore = "基准：需真实凭证 + 真金白银，手动跑（见模块头）"]
fn bench_report() {
    // 与改写进程级 env 的用例串行：否则子进程可能读到被临时清掉的 key。
    let _env_guard = env_lock();
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
    // 汇总 JSON：每条结果追加一条；`YUSHAN_BENCH_OUT` 可指向跨运行复用的固定路径。
    let summary_path = bench_summary_path(&root);
    // 协议面档位：决定记录表里的「协议」列（base 由 `YUSHAN_BENCH_BASE` 覆盖）。
    let protocol = protocol_from_env();

    println!(
        "\n基准：{} 条提示 × {repeats} 次   工作区 {}\n临时目录 {}\n",
        PROMPTS.len(),
        workspace.display(),
        root.display()
    );
    println!(
        "协议面：{}   base：{}",
        protocol.label(),
        base_override()
            .map(|(_, base)| base)
            .unwrap_or_else(|| "（沿用 YUSHAN_API_BASE）".to_string())
    );
    if base_override().is_some() && !base_override_effective() {
        println!(
            "⚠ 设了 {}，但 env 里没有 API key —— 应用会用 /login 存下的 provider base，本次不是协议面对照。",
            BASE_ENV
        );
    }

    let mut summary: Vec<Summary> = Vec::new();
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
        // 质量判据：没有则该提示的质量位记「未设判据」。
        let spec = quality_spec(name);
        let mut readings = Vec::new();
        for i in 0..repeats {
            let run_dir = root.join(format!("{}-{}", sanitize(name), i));
            std::fs::create_dir_all(&run_dir).expect("建运行目录");
            match run_once(&bin, &workspace, &run_dir, task, timeout) {
                Some(mut r) => {
                    // 质量位：机械核对（证据齐不齐、引用指不指得到真位置）。
                    r.quality = quality_of(&answer_text(&run_dir), &workspace, spec);
                    append_json_record(&summary_path, bench_record(name, i + 1, protocol, &r));
                    let (t, g, s) = r.split();
                    println!(
                        "  run {}  协议 {}  质量 {}  墙钟 {}s  轮 {:<3} ttft {}%  gen {}%  tools {}%   \
                         ({}/{}/{} ms)  attempts {}  retries {}  retry_wait {}ms  cache {}",
                        i + 1,
                        protocol.label(),
                        r.quality.label(),
                        secs(r.wall_ms),
                        r.rounds,
                        t,
                        g,
                        s,
                        r.ttft_ms,
                        r.gen_ms,
                        r.tools_ms,
                        r.attempts,
                        r.retried_attempts,
                        r.retry_wait_ms,
                        cache_pct(r.cache_hit, r.cache_miss),
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
        let attempts: Vec<u128> = readings.iter().map(|r| r.attempts).collect();
        let retries: Vec<u128> = readings.iter().map(|r| r.retried_attempts).collect();
        let retry_waits: Vec<u128> = readings.iter().map(|r| r.retry_wait_ms).collect();
        println!(
            "  **中位**  墙钟 {}s  轮 {}  ({}/{}/{} ms)  attempts {}  retries {}  \
             retry_wait {}ms\n",
            secs(median(&walls)),
            median(&rounds),
            median(&ttfts),
            median(&gens),
            median(&tools),
            median(&attempts),
            median(&retries),
            median(&retry_waits),
        );
        // 质量位取各次运行里的**最差**值（`Fail` 最重）。
        let quality = readings
            .iter()
            .map(|r| r.quality)
            .max()
            .unwrap_or(Quality::Unjudged);
        summary.push((
            (*name).to_string(),
            median(&walls),
            median(&rounds),
            median(&ttfts),
            median(&gens),
            median(&tools),
            median(&attempts),
            median(&retries),
            median(&retry_waits),
            readings.iter().map(|r| r.cache_hit).sum(),
            readings.iter().map(|r| r.cache_miss).sum(),
            quality,
        ));
    }

    println!("=== 汇总（中位）===");
    println!("{}", summary_header());
    for row in &summary {
        println!("{}", summary_row(row, protocol));
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
        println!(
            "  逐轮读数：{}/<提示>-<序号>/logs/{{yushan.log,debug-*.jsonl}}",
            root.display()
        );
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

/// debug JSONL 是基准的首选读数：同一份数据里同时得到轮耗时、HTTP 尝试、退避与缓存。
///
/// 变异：把 `round_end` 的 `rounds += 1` 删掉 ⇒ 第一条断言红；把 `retried` 判断
/// 改成恒真 ⇒ 第二条断言红。
#[test]
fn parse_debug_text_collects_attempts_and_cache() {
    let text = r#"{"ev":"run_start","pid":42}
{"ev":"round_start","round":1}
{"ev":"model_attempt","attempt":1,"retried":false}
{"ev":"round_usage","purpose":"turn","gen_ai.usage.input_tokens":100,"gen_ai.usage.cache_read.input_tokens":90}
{"ev":"round_end","ttft_ms":10,"gen_ms":20,"tools_ms":30}
{"ev":"round_start","round":2}
{"ev":"model_attempt","attempt":1,"retried":true,"backoff_ms":500}
{"ev":"model_attempt","attempt":2,"retried":false}
{"ev":"round_usage","purpose":"turn","gen_ai.usage.input_tokens":20,"gen_ai.usage.cache_read.input_tokens":15}
{"ev":"round_end","ttft_ms":11,"gen_ms":21,"tools_ms":31}
{"ev":"run_end"}
{"ev":"round_end","ttft_ms":999,"gen_ms":999,"tools_ms":999}
"#;
    let reading = parse_debug_text(text).expect("应有读数");
    assert_eq!(reading.rounds, 2, "首条 run_end 之后不得再计数");
    assert_eq!(
        (reading.ttft_ms, reading.gen_ms, reading.tools_ms),
        (21, 41, 61)
    );
    assert_eq!(reading.attempts, 3);
    assert_eq!(reading.retried_attempts, 1);
    assert_eq!(reading.retry_wait_ms, 500);
    assert_eq!((reading.cache_hit, reading.cache_miss), (105, 15));
}

/// 同一 `YUSHAN_LOG_DIR` 里有父/子两个 debug 文件时，必须按 `run_start.pid` 选父进程。
#[test]
fn debug_file_for_pid_uses_run_start_owner() {
    let dir = temp_dir("debug-select");
    std::fs::write(
        dir.join("debug-child.jsonl"),
        "{\"ev\":\"run_start\",\"pid\":99}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("debug-parent.jsonl"),
        "{\"ev\":\"run_start\",\"pid\":42}\n",
    )
    .unwrap();

    let selected = debug_file_for_pid(&dir, 42).expect("应选中父进程文件");
    assert_eq!(
        selected.file_name().and_then(|name| name.to_str()),
        Some("debug-parent.jsonl")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ─────────────────────────────── 离线单测辅助 ───────────────────────────────

/// 进程级 env 改写的互斥锁。
///
/// 集成测试拿不到 app 的 `test_env` 模块，这里按同一约定自备一份：
/// 改写进程级 env 的用例必须先拿这把锁，彼此串行。
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// 进程级 env 的原值记录，`Drop` 时恢复。
///
/// 用 `Drop` 而非测试末尾手动恢复：断言 panic 时也能还原，不污染同 binary 内的其他用例。
struct EnvRestore {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvRestore {
    /// 记录 `keys` 在当前进程中的原值。
    fn capture(keys: &[&'static str]) -> Self {
        EnvRestore {
            saved: keys.iter().map(|k| (*k, std::env::var_os(k))).collect(),
        }
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        // SAFETY: 调用方持有 env_lock，独占进程环境变量的修改与恢复。
        unsafe {
            for (key, value) in &self.saved {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

/// 协议面开关的解析：未设置 ⇒ `chat`；`responses` ⇒ 生效；非法值 ⇒ 回退 `chat`。
#[test]
fn protocol_parse_falls_back_to_chat() {
    assert_eq!(parse_protocol(None), Protocol::Chat, "未设置 ⇒ chat");
    assert_eq!(parse_protocol(Some("chat")), Protocol::Chat);
    assert_eq!(
        parse_protocol(Some("responses")),
        Protocol::Responses,
        "responses 档必须生效"
    );
    assert_eq!(parse_protocol(Some(" Responses ")), Protocol::Responses);
    // 非法值 ⇒ 回退默认档，不 panic。
    assert_eq!(parse_protocol(Some("grpc")), Protocol::Chat);
    assert_eq!(parse_protocol(Some("response")), Protocol::Chat);
    assert_eq!(parse_protocol(Some("")), Protocol::Chat);
}

/// 开关确实从 `YUSHAN_BENCH_PROTOCOL` 读（不只是纯函数自洽）。
#[test]
fn protocol_env_switch_takes_effect() {
    let _guard = env_lock();
    let _restore = EnvRestore::capture(&[PROTOCOL_ENV]);
    // SAFETY: 持有 env_lock，独占进程环境变量的修改。
    unsafe { std::env::remove_var(PROTOCOL_ENV) };
    assert_eq!(protocol_from_env(), Protocol::Chat, "未设置 ⇒ chat");
    // SAFETY: 同上。
    unsafe { std::env::set_var(PROTOCOL_ENV, "responses") };
    assert_eq!(
        protocol_from_env(),
        Protocol::Responses,
        "设为 responses ⇒ 生效"
    );
    // SAFETY: 同上。
    unsafe { std::env::set_var(PROTOCOL_ENV, "not-a-protocol") };
    assert_eq!(protocol_from_env(), Protocol::Chat, "非法值 ⇒ 回退 chat");
}

/// 记录表：列名与渲染必须齐（墙钟 / 轮数 / ttft / gen / tools / 质量位）。
#[test]
fn summary_table_header_and_row_line_up() {
    let header = summary_header();
    for column in [
        "提示",
        "协议",
        "墙钟s",
        "轮",
        "ttft_ms",
        "gen_ms",
        "tools_ms",
        "质量",
        "attempts",
        "retries",
        "retry_wait",
        "cache",
    ] {
        assert!(header.contains(column), "表头缺列：{column}");
    }
    let row = (
        "fact/1".to_string(),
        12_300,
        3,
        1_200,
        3_400,
        800,
        3,
        1,
        500,
        90,
        10,
        Quality::Pass,
    );
    let rendered = summary_row(&row, Protocol::Responses);
    assert_eq!(
        rendered.split_whitespace().count(),
        header.split_whitespace().count(),
        "表头与数据行的列数必须一致：\n{header}\n{rendered}"
    );
    assert!(rendered.contains("fact/1"), "行里要有提示名");
    assert!(rendered.contains("responses"), "行里要有协议面");
    assert!(rendered.contains("Q+"), "行里要有质量位");
    assert!(rendered.contains("12.3"), "行里要有墙钟秒数");
    assert!(rendered.contains("1200"), "行里要有 ttft 毫秒");
    assert!(rendered.contains("3400"), "行里要有 gen 毫秒");
    assert!(rendered.contains("800"), "行里要有 tools 毫秒");
}

/// 证据引用的抽取：只认「像仓库路径」的 `文件:行号`。
#[test]
fn citations_only_take_file_line_references() {
    let found = citations("见 docs/a.md:2 与 crates/x/src/lib.rs:10-12，还有 http://h:8080/p");
    assert_eq!(found.len(), 2, "端口号不是文件行号：{found:?}");
    assert_eq!(
        found[0],
        Citation {
            path: "docs/a.md".to_string(),
            line: 2
        }
    );
    assert_eq!(
        found[1],
        Citation {
            path: "crates/x/src/lib.rs".to_string(),
            line: 10
        }
    );
    // 全角冒号也认。
    let full = citations("docs/a.md：7");
    assert_eq!(full.len(), 1);
    assert_eq!(full[0].line, 7);
}

/// 质量位：机械核对 —— 证据齐不齐、引用指不指得到真位置。
#[test]
fn quality_bit_checks_evidence_against_the_workspace() {
    let root = temp_dir("quality");
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::create_dir_all(root.join("crates/demo/src")).unwrap();
    std::fs::write(root.join("docs/a.md"), "1\n2\n3\n").unwrap();
    std::fs::write(root.join("crates/demo/src/lib.rs"), "1\n2\n3\n").unwrap();

    let spec = QualitySpec { min_citations: 2 };

    // 证据齐、引用都指得到 ⇒ 通过
    let good = "见 docs/a.md:2 与 crates/demo/src/lib.rs:3";
    assert_eq!(quality_of(good, &root, Some(&spec)), Quality::Pass);
    // 引用数不足 ⇒ 不过
    assert_eq!(
        quality_of("见 docs/a.md:2", &root, Some(&spec)),
        Quality::Fail
    );
    // 引用指向不存在的行 ⇒ 部分过
    assert_eq!(
        quality_of(
            "见 docs/a.md:99 与 crates/demo/src/lib.rs:3",
            &root,
            Some(&spec)
        ),
        Quality::Partial
    );
    // 空答案 ⇒ 不过
    assert_eq!(quality_of("   \n", &root, Some(&spec)), Quality::Fail);
    // 无判据又没有可核对的引用 ⇒ 未设判据
    assert_eq!(quality_of("结论如下", &root, None), Quality::Unjudged);

    let _ = std::fs::remove_dir_all(&root);
}

/// 质量判据挂在正确的提示上。
#[test]
fn quality_specs_cover_the_judgeable_prompts() {
    let fact = quality_spec("fact/1").expect("fact/1 是可判定任务");
    assert_eq!(fact.min_citations, 6, "3 处 × (文档 + 代码) 各一条");
    assert!(quality_spec("daily/2").is_some(), "daily/2 要文件与行号");
    assert!(quality_spec("daily/1").is_none(), "没有机械判据 ⇒ 不挂");
}

/// base 覆盖的生效条件：env 里要有**完整**凭证（base + key）。
#[test]
fn base_override_needs_complete_env_credentials() {
    let _guard = env_lock();
    let _restore = EnvRestore::capture(&[BASE_ENV, "YUSHAN_API_KEY", "OPENAI_API_KEY"]);
    // SAFETY: 持有 env_lock，独占进程环境变量的修改。
    unsafe {
        std::env::remove_var(BASE_ENV);
        std::env::remove_var("YUSHAN_API_KEY");
        std::env::remove_var("OPENAI_API_KEY");
    }
    assert!(base_override().is_none(), "没设 base ⇒ 无覆盖");
    assert!(!base_override_effective(), "没设 base ⇒ 不生效");
    // SAFETY: 同上。
    unsafe { std::env::set_var(BASE_ENV, "http://127.0.0.1:9/v1") };
    assert!(base_override().is_some(), "设了 base ⇒ 有覆盖");
    assert!(
        !base_override_effective(),
        "只有 base 没有 key ⇒ 不生效（应用会用 /login 的 provider base）"
    );
    // SAFETY: 同上。
    unsafe { std::env::set_var("YUSHAN_API_KEY", "sk-test") };
    assert!(base_override_effective(), "base + key ⇒ 生效");
}

// ─────────────────────────────── 汇总落盘单测 ───────────────────────────────

/// `Reading` → JSON：三个新字段齐全且为数字；`run_reading` 缺失时写 `null`。
#[test]
fn bench_summary_json_carries_run_reading_fields() {
    let reading = Reading {
        tpr_x100: 151,
        input_med: 121_344,
        single_call_rounds: 2,
        run_reading_seen: true,
        ..Reading::default()
    };
    let value = reading.to_json();
    assert_eq!(value["tpr_x100"].as_u64(), Some(151));
    assert_eq!(value["input_med"].as_u64(), Some(121_344));
    assert_eq!(value["single_call_rounds"].as_u64(), Some(2));
    assert!(value["tpr_x100"].is_u64(), "tpr_x100 必须是 JSON 数字");
    assert!(value["input_med"].is_u64(), "input_med 必须是 JSON 数字");
    assert!(
        value["single_call_rounds"].is_u64(),
        "single_call_rounds 必须是 JSON 数字"
    );

    // 没有 run_reading ⇒ 三个字段为 null（与表格的「-」同义），不伪装成 0。
    let absent = Reading::default().to_json();
    for key in ["tpr_x100", "input_med", "single_call_rounds"] {
        assert!(absent[key].is_null(), "缺失 run_reading 时 {key} 应为 null");
    }
}

/// `YUSHAN_BENCH_OUT` 存在且非空 ⇒ 固定路径生效，且跨次写入追加到同一个数组。
#[test]
fn bench_summary_path_honors_out_env() {
    let _guard = env_lock();
    let _restore = EnvRestore::capture(&[BENCH_OUT_ENV]);
    let dir = temp_dir("bench-out-env");
    let fixed = dir.join("fixed-summary.json");
    let run_dir = dir.join("run");
    // SAFETY: 持有 env_lock，独占进程环境变量的修改。
    unsafe { std::env::set_var(BENCH_OUT_ENV, &fixed) };

    assert_eq!(bench_summary_path(&run_dir), fixed, "非空 ⇒ 固定路径");

    let reading = Reading {
        rounds: 3,
        tpr_x100: 133,
        input_med: 900,
        single_call_rounds: 1,
        run_reading_seen: true,
        ..Reading::default()
    };
    append_json_record(
        &bench_summary_path(&run_dir),
        bench_record("fact/1", 1, Protocol::Chat, &reading),
    );
    append_json_record(
        &bench_summary_path(&run_dir),
        bench_record("fact/1", 2, Protocol::Chat, &reading),
    );

    let text = std::fs::read_to_string(&fixed).expect("固定路径应已落盘");
    let value: Value = serde_json::from_str(&text).expect("合法 JSON");
    let records = value.as_array().expect("顶层必须是数组");
    assert_eq!(records.len(), 2, "两次写入必须追加而不是覆盖");
    assert_eq!(records[0]["name"], "fact/1");
    assert_eq!(records[0]["repeat"], 1);
    assert_eq!(records[0]["protocol"], "chat");
    assert_eq!(records[1]["repeat"], 2);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 未设置（或空串）⇒ 写本次 run 目录下的 `bench-summary.json`。
#[test]
fn bench_summary_defaults_to_run_dir() {
    let _guard = env_lock();
    let _restore = EnvRestore::capture(&[BENCH_OUT_ENV]);
    let dir = temp_dir("bench-out-default");
    // SAFETY: 持有 env_lock，独占进程环境变量的修改。
    unsafe { std::env::remove_var(BENCH_OUT_ENV) };
    assert_eq!(bench_summary_path(&dir), dir.join("bench-summary.json"));

    // 空串视同未设置。
    // SAFETY: 同上。
    unsafe { std::env::set_var(BENCH_OUT_ENV, "  ") };
    assert_eq!(bench_summary_path(&dir), dir.join("bench-summary.json"));

    // 未设置时确实写到 run 目录。
    let reading = Reading::default();
    append_json_record(
        &bench_summary_path(&dir),
        bench_record("daily/1", 1, Protocol::Chat, &reading),
    );
    let value: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("bench-summary.json")).expect("应写到 run 目录"),
    )
    .expect("合法 JSON");
    assert_eq!(value.as_array().unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 找不到 `run_reading` ⇒ 三个字段回落为 0 / null，且解析不 panic。
#[test]
fn parse_debug_without_run_reading_falls_back_without_panic() {
    let text = r#"{"ev":"run_start","pid":42}
{"ev":"round_end","ttft_ms":10,"gen_ms":20,"tools_ms":30,"tool_calls":1}
{"ev":"round_end","ttft_ms":11,"gen_ms":21,"tools_ms":31,"tool_calls":3}
{"ev":"run_end"}
"#;
    let reading = parse_debug_text(text).expect("有 round_end 就应有读数");
    assert_eq!(reading.rounds, 2);
    assert_eq!(
        (
            reading.tpr_x100,
            reading.input_med,
            reading.single_call_rounds
        ),
        (0, 0, 0)
    );
    assert!(!reading.run_reading_seen);
    let value = reading.to_json();
    for key in ["tpr_x100", "input_med", "single_call_rounds"] {
        assert!(value[key].is_null(), "无 run_reading 时 {key} 应为 null");
    }
}

/// 生产 schema：`run_reading` 同时带 `line` 与顶层数字字段；只有 `line` 时
/// 也必须能反解出 `tpr_x100` / `input_med`（bench 端兜底）。
///
/// 变异：`absorb_run_reading` 只读顶层键 ⇒ 第二个用例（只有 `line`）变红，
/// 回到「有记录却读成假 0」。
#[test]
fn parse_debug_absorbs_run_reading_record() {
    // 形态一：生产端当前的真实 schema —— `line`（人读出口）+ 顶层数字字段。
    let text = r#"{"ev":"run_start","pid":42}
{"ev":"round_end","ttft_ms":10,"gen_ms":20,"tools_ms":30,"tool_calls":1}
{"ev":"round_end","ttft_ms":11,"gen_ms":21,"tools_ms":31,"tool_calls":2}
{"ev":"run_reading","turn":1,"line":"turn=1 rounds=2 tools=3 tpr_x100=150 ttft_ms=21 gen_ms=41 tools_ms=61 input_med=121344 cache_pct=90","tpr_x100":150,"input_med":121344,"rounds":2,"tools":3,"single_call_rounds":7}
{"ev":"run_end"}
"#;
    let reading = parse_debug_text(text).expect("应有读数");
    assert_eq!(reading.tpr_x100, 150);
    assert_eq!(reading.input_med, 121_344);
    assert_eq!(reading.single_call_rounds, 7, "记录自带的值优先");
    assert!(reading.run_reading_seen);

    // 形态二：只有 `line`、没有顶层数字键 —— 仍必须从文本反解出正确值。
    let line_only = r#"{"ev":"run_start","pid":42}
{"ev":"round_end","ttft_ms":10,"gen_ms":20,"tools_ms":30,"tool_calls":1}
{"ev":"round_end","ttft_ms":11,"gen_ms":21,"tools_ms":31,"tool_calls":2}
{"ev":"run_reading","turn":1,"line":"turn=1 rounds=2 tools=3 tpr_x100=150 ttft_ms=21 gen_ms=41 tools_ms=61 input_med=121344 cache_pct=90"}
{"ev":"run_end"}
"#;
    let reading = parse_debug_text(line_only).expect("应有读数");
    assert_eq!(
        (reading.tpr_x100, reading.input_med),
        (150, 121_344),
        "顶层缺失时必须从 line 反解，不能假 0"
    );
    assert_eq!(
        reading.single_call_rounds, 1,
        "line 里没有该字段 ⇒ 由 round_end.tool_calls == 1 派生"
    );
    assert!(reading.run_reading_seen);

    // 记录在 `run_end` 之后也要吸收（落盘顺序不保证）。
    let after = r#"{"ev":"run_start","pid":42}
{"ev":"round_end","ttft_ms":10,"gen_ms":20,"tools_ms":30,"tool_calls":1}
{"ev":"run_end"}
{"ev":"run_reading","turn":1,"line":"turn=1 rounds=1 tools=1 tpr_x100=100 ttft_ms=10 gen_ms=20 tools_ms=30 input_med=777 cache_pct=n/a"}
"#;
    let reading = parse_debug_text(after).expect("应有读数");
    assert_eq!((reading.tpr_x100, reading.input_med), (100, 777));
    assert_eq!(
        reading.single_call_rounds, 1,
        "记录没带该字段 ⇒ 由 round_end.tool_calls == 1 派生"
    );
}

/// 汇总文件损坏时先备份、再写新数组；**原内容不得被静默清空**。
///
/// 变异：把备份分支改回 `Vec::new()` 直接覆盖 ⇒ 备份文件不存在 / 内容断言变红。
#[test]
fn append_json_record_backs_up_corrupt_file_instead_of_overwriting() {
    let dir = temp_dir("bench-corrupt");
    for (file_name, payload) in [
        ("broken.json", &b"{not json"[..]),
        ("not-array.json", &b"{\"keep\":1}"[..]),
    ] {
        let path = dir.join(file_name);
        std::fs::write(&path, payload).unwrap();

        append_json_record(&path, json!({"name": "new"}));

        let backup_prefix = format!("{file_name}.corrupt-");
        let backups: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&backup_prefix))
            })
            .collect();
        assert_eq!(backups.len(), 1, "{file_name} 应恰好备份一次：{backups:?}");
        assert_eq!(
            std::fs::read(&backups[0]).unwrap(),
            payload,
            "备份必须逐字节保留原内容"
        );

        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).expect("新文件合法");
        assert_eq!(value.as_array().unwrap().len(), 1, "新数组只含本次记录");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// stdout 表格是字节稳定的契约：表头与数据行的精确输出不得漂移。
#[test]
fn summary_table_output_is_byte_stable() {
    assert_eq!(
        summary_header(),
        "提示         协议             墙钟s     轮    ttft_ms     gen_ms   tools_ms   质量  attempts  retries   retry_wait   cache"
    );

    let row = (
        "fact/1".to_string(),
        12_300,
        3,
        1_200,
        3_400,
        800,
        3,
        1,
        500,
        90,
        10,
        Quality::Pass,
    );
    assert_eq!(
        summary_row(&row, Protocol::Responses),
        "fact/1     responses     12.3     3       1200       3400        800   Q+         3        1          500     90%"
    );
}
