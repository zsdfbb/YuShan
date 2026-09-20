//! 入口：模式分发（TUI / `-p` / `--json`）+ 接线器组装（设计 §7 初始化顺序）。
//!
//! # 线程模型（TUI 模式）
//!
//! ```text
//! 主线程（**不在 runtime 上下文里**）        app 线程（tokio multi-thread worker）
//! ─────────────────────────────────         ────────────────────────────────────
//! ys_tui_coding::run(view₀, rx_out,          app_loop::run(agent, wiring, …)
//!                    tx_req, tx_boundary)      loop { req = request_rx.recv() }
//! ```
//!
//! `ys_tui_coding::run` 用 `Sender::blocking_send` 投请求，**必须在非 async
//! 线程调用** —— 故 `main` 是普通 `fn`：先建 runtime（异步初始化 `Wiring`），
//! `rt.spawn(app_loop)`，再在**当前线程**跑阻塞的 UI 循环。UI 退出 → `tx_req`
//! 被 drop → app 线程 `recv()` 得 `None` → 自然收摊。
//!
//! 初始化顺序（含 R7 的阻塞提前返回）：
//!
//! ```text
//! 0. from_env 建 web 配置 → 启动期清理（仅 Primary 档）
//!    → 装工具表（toolset 探测一次）→ 降级提示打 stderr → 由工具表派生 system prompt
//! 1. 建 ChannelSink（policy 已定）+ 建 Agent（挂工具与系统提示）
//! 2. 【阻塞】model + Wiring 构造   ← 可能失败并提前返回（turn 恒 0 也无所谓）
//! 3. 构造 view₀
//! 4. 建三条信道 request / boundary / outbound
//! 5. spawn app 线程 + 当前线程跑 UI
//! ```
//!
//! 第 0 步在**建 runtime 之前**：工具表既进 agent，也派生提示词清单 —— 同一份表，
//! 顺序自然一致（设计 §1.2 启动期 / §1.5 提示词来源）。
//!
//! 注意 **Agent 先于 Wiring 建**（`run_interactive` 的实际顺序）：二者互不依赖，
//! 而 `view₀` 要同时借用它们，故都放在 view₀ 之前。承重的不变量是「`begin_turn`
//! 只可能在 `rt.spawn` 之后发生」（`view₀` 因此必然看到 turn 恒 0），与这两者
//! 的先后无关。

mod app_loop;
mod capabilities;
mod channel;
mod config;
mod logging;
mod prompt;
mod provider;
mod skills;
mod state;
mod status;
mod toolset;
mod view;
mod wiring;

/// 测试专用：进程级 env 互斥与恢复，供各模块测试复用。
#[cfg(test)]
mod test_env;

use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use ys_event::AgentEvent;
use ys_loop::{AgentInput, LoopError, RunResult};
use ys_model_openai_compat::{OpenAICompatibleConfig, OpenAICompatibleModel};
use ys_protocol::{Boundary, Envelope, LifecyclePolicy, Outbound, Request};
use ys_runtime::{Agent, AgentBuilder, BuildError};
use ys_tool::Tool;
use ys_tools_web::WebConfig;
use ys_tui_coding::CodingView;

use channel::{ChannelSink, ChannelStatsHandle};
use wiring::Wiring;

/// 运行模式。
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    /// 单次 print 模式：边生成边打印文本增量。
    Print(String),
    /// JSON 事件模式：逐行序列化 [`Envelope`]。
    Json(String),
    /// 交互式 TUI：真正的 UI 在 `ys-tui-coding`（独立 crate），本 crate 只接线。
    Interactive,
}

/// 解析后的命令行参数。
#[derive(Debug, PartialEq, Eq)]
struct Args {
    mode: Mode,
    /// `--stats`：显式要求把背压读数摘要写到 stderr（无论有无背压）。
    stats: bool,
}

/// 摘掉紧随模式 flag 之后、任务文本之前的可选 `--stats`。
///
/// 只认**紧邻**位置（`-p --stats "任务"`）：任务文本一旦开始，`--stats`
/// 就是文本的一部分（`-p "任务" --stats` → 文本 `"任务 --stats"`），
/// 从而保住「任务文本逐字保留」的既有语义。
fn split_stats_flag(rest: &[String]) -> (bool, &[String]) {
    match rest.first().map(String::as_str) {
        Some("--stats") => (true, &rest[1..]),
        _ => (false, rest),
    }
}

/// 解析 argv（不含 argv[0]）。
///
/// **只在首个参数位置判定模式 flag**：任务文本本身可能以 `-` 开头，
/// 例如 `-p --help me` 的任务文本就是 `--help me`。
/// 唯一的例外是 `--stats`——见 [`split_stats_flag`]。
fn parse_args(argv: &[String]) -> Result<Args, String> {
    let Some(first) = argv.first().map(String::as_str) else {
        return Ok(Args {
            mode: Mode::Interactive,
            stats: false,
        });
    };

    match first {
        "-p" => {
            let (stats, rest) = split_stats_flag(&argv[1..]);
            let task = rest.join(" ");
            if task.is_empty() {
                Err("-p 需要一个任务参数，例如：ys-coding-agent -p \"修复这个 bug\"".into())
            } else {
                Ok(Args {
                    mode: Mode::Print(task),
                    stats,
                })
            }
        }
        "--json" => {
            let (stats, rest) = split_stats_flag(&argv[1..]);
            let task = rest.join(" ");
            if task.is_empty() {
                Err("--json 需要一个任务参数，例如：ys-coding-agent --json \"修复这个 bug\"".into())
            } else {
                Ok(Args {
                    mode: Mode::Json(task),
                    stats,
                })
            }
        }
        // 未知 flag：仅当它出现在**首个**位置时才算 flag（其余位置属任务文本）。
        // 单独的 `-` 视为普通参数（约定：读 stdin 的占位，本轮不支持）。
        f if f.starts_with('-') && f != "-" => Err(format!(
            "未知参数：{f}（可用：-p [--stats] <任务> | --json [--stats] <任务>）"
        )),
        // 无 flag：交互式（与旧实现一致）。
        _ => Ok(Args {
            mode: Mode::Interactive,
            stats: false,
        }),
    }
}

/// 交互模式信道容量：设计 §8 back-of-envelope 给交互式建议 1024
/// （约 112 KiB 内存；后台长任务可另配 4096）。
/// 可用环境变量 `YUSHAN_CHANNEL_CAPACITY` 覆盖。
const DEFAULT_CHANNEL_CAPACITY: usize = 1024;

/// 信道容量下限。低于此值时每个事件几乎必然撞满 → 每事件都走异步慢路径
/// （正确但性能差），且易诱发配置误用。设计容量本就不该这么小，
/// 故 clamp 到 16 并在 stderr 警告一次（env 覆盖路径）。
const MIN_CHANNEL_CAPACITY: usize = 16;

/// Outbound（app → UI）信道容量。UI 每个 poll 周期排空一次，积压量很小。
const OUTBOUND_CAPACITY: usize = 1024;
/// Request（UI → app）信道容量。UI 用 `blocking_send`，容量给足避免误阻塞。
const REQUEST_CAPACITY: usize = 64;
/// Boundary（UI → `BasicLoop`）信道容量。回合结束后的残留由 app 侧清空。
const BOUNDARY_CAPACITY: usize = 64;
/// UI 退出后，给 app 线程收摊的宽限时长（超时即连 runtime 一起丢弃）。
const APP_THREAD_GRACE: Duration = Duration::from_secs(1);

/// 把请求容量 clamp 到 [`MIN_CHANNEL_CAPACITY`]。
///
/// 注意：死锁已由「自由函数总走慢路径」结构性修复，clamp **不是**正确性补丁，
/// 只是避免把容量配成 1 这类性能误用的护栏。死锁回归测试直接用
/// `ChannelSink::new(1/2, …)` 覆盖真正的极小容量。
#[inline]
fn clamp_capacity(requested: usize) -> usize {
    requested.max(MIN_CHANNEL_CAPACITY)
}

fn channel_capacity() -> usize {
    let requested = std::env::var("YUSHAN_CHANNEL_CAPACITY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_CHANNEL_CAPACITY);
    let clamped = clamp_capacity(requested);
    if clamped != requested {
        eprintln!(
            "warning: YUSHAN_CHANNEL_CAPACITY={requested} 低于下限 {MIN_CHANNEL_CAPACITY}，已 clamp 到 {clamped}"
        );
    }
    clamped
}

/// 把信封序列化为一行 JSON 写入（供 `--json` 模式）。输出后换行。
///
/// 直接 `to_writer`（不经过中间 `String`），最后补一个 `\n` —— 一行一个信封。
fn write_json_envelope<W: Write>(w: &mut W, env: &Envelope) -> io::Result<()> {
    serde_json::to_writer(&mut *w, env).map_err(io::Error::other)?;
    w.write_all(b"\n")
}

/// 从事件中提取可打印的文本增量（供 `-p` 模式）。
///
/// 返回 `Some(text)` 当且仅当该事件是 [`AgentEvent::ModelTextDelta`]。
fn print_delta_text(env: &Envelope) -> Option<&str> {
    match &env.event {
        AgentEvent::ModelTextDelta { text } => Some(text),
        _ => None,
    }
}

/// 终局事件：消费循环的退出条件。
fn is_terminal(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::RunFinished { .. } | AgentEvent::RunFailed { .. }
    )
}

/// 消费事件流，直到收到终局事件（`RunFinished` / `RunFailed`）。
///
/// **终止条件是终局事件，不是信道关闭** —— sender 在 Agent 里，
/// Agent 不 drop 信道就不会关闭；若等关闭会死锁。
///
/// 每个事件 `on_event` 写出后 `flush()`（保证 `-p` 增量实时可见）。
async fn consume_events<W, F>(
    mut rx: mpsc::Receiver<Envelope>,
    mut w: W,
    mut on_event: F,
) -> io::Result<()>
where
    W: Write,
    F: FnMut(&mut W, &Envelope) -> io::Result<()>,
{
    while let Some(env) = rx.recv().await {
        on_event(&mut w, &env)?;
        w.flush()?;
        if is_terminal(&env.event) {
            return Ok(());
        }
    }
    // 信道关闭（正常路径不依赖此分支：agent 不 drop 信道）——不 panic，正常收尾。
    w.flush()
}

/// 组装 agent（无状态执行器）。
///
/// ADR-0010：会话、事件出口与模型不再进入 agent —— 它们归 [`Wiring`]，
/// 运行时经端口传入。
///
/// **工具表由调用方装配后传入**（[`toolset::build_tools`] 唯一装配：探测一次 +
/// 固定顺序）：同一份表既进 `ToolRegistry`，也派生系统提示词的工具清单 ——
/// 二者因此不可能漂移（设计 §1.5）。
///
/// **`limits` 必须由调用方传入**（U6b 接线）：此前本函数从不调 `.limits()`，
/// `AgentBuilder` 落回 `RunLimits::default()` ⇒ `context_window` 恒为 128_000 ——
/// 这正是「1M 模型在 11 万 token 就被压缩」的根因。传给 `build_agent` 的
/// `context_window` 只是**启动时初值**：TUI 模式下 `app_loop` 每回合按当前 model
/// 重算（`/login` / `/model` 能在运行中换到 16 倍窗口的模型）。
fn build_agent(
    system_prompt: String,
    workspace: PathBuf,
    tools: Vec<Box<dyn Tool>>,
    limits: ys_component::RunLimits,
) -> Result<Agent, BuildError> {
    AgentBuilder::new()
        .tools(tools)
        .system_prompt(system_prompt)
        .working_dir(workspace.clone(), workspace)
        .approval(ys_tool::AutoApprove)
        .limits(limits)
        .build()
}

/// 从 [`Config`] 解析出这次运行的执行限制。
///
/// # 为什么抽成独立函数
///
/// **只为可测**：这段装配若留在 `main()` 里，测试够不着 —— `main()` 会真读环境变量、
/// 真起 runtime、真进 TUI。而它恰恰是本刀（U6b）修复的**接线**那一步：
/// [`build_agent`] 已能正确应用传入的 `limits`（有单测钉住），但「取 `config` 的值
/// 再传进去」若被误写成 `RunLimits::default()`，**全部测试依旧全绿**，
/// 而回归的正是「1M 模型在 11 万 token 就被压缩」这个 bug。抽出来后可被单测直接调用。
///
/// # 依赖顺序
///
/// 调用方必须在 **provider/model 恢复之后**调用：`context_window()` 按 `config.model`
/// 查表（`config.json` 覆盖 > 内置表 > 默认 128_000），恢复逻辑会改写 `config.model`。
///
/// `RunLimits` 是 `#[non_exhaustive]`：跨 crate **不能**用结构体字面量，
/// 只能 `default()` + 字段赋值。
fn resolve_limits(config: &config::Config) -> ys_component::RunLimits {
    let mut limits = ys_component::RunLimits::default();
    limits.context_window = config.context_window();
    limits.compaction = config.compact_policy();
    limits
}

/// 每轮**固定开销**的 token 估算：system prompt + 工具表的 JSON spec。
///
/// 压缩触发（`ys-loop`）此前只算 session 消息，这两项是盲区；skill 目录入场后
/// 缺口从 ~1.1k 涨到 ~9.6k（设计 §选择理由 E）。量一次、交给 `RunLimits`。
///
/// **估算器不引额外依赖**：复用 `ys_loop::token`（全仓唯一的估算实现）。
/// `serde_json::to_string` 失败（不可能：`ToolSpec` 是可序列化的纯数据）时按
/// 空串计 —— 宁可低估，也不要让启动失败。
fn fixed_prompt_tokens(system_prompt: &str, specs: &[ys_tool::ToolSpec]) -> usize {
    ys_loop::token::estimate_text_tokens(system_prompt)
        + ys_loop::token::estimate_text_tokens(&serde_json::to_string(specs).unwrap_or_default())
}

/// R5：固定开销已经**不小于**压缩触发点 ⇒ 压缩即便触发也帮不上（消息已到底），
/// 或永不触发而溢出。给一条可操作的启动期提示（调大窗口 / 减少 skill）。
///
/// **只诊断、不改变行为**：触发条件与 `trigger_at` 的语义都不动（设计 §选择理由 E
/// 明确「扣减发生在比较的输入端」，`trigger_at` 原样保留）。
fn fixed_cost_note(limits: &ys_component::RunLimits) -> Option<String> {
    let trigger = limits.compaction.trigger_at(limits.context_window);
    if limits.fixed_prompt_tokens < trigger {
        return None;
    }
    Some(format!(
        "[yushan] 每轮固定开销（system prompt + 工具表）≈ {} token，已达压缩触发点 {} token \
         （context_window = {}）—— 压缩无从下手，长会话会直接撞窗口。请调大 context_window、\
         或给部分 skill 标 `disable-model-invocation` 以减少 skill 目录。",
        limits.fixed_prompt_tokens, trigger, limits.context_window
    ))
}

/// 自定义 prompt（`.yushan/SYSTEM.md`）命中时的**代价说明**。
///
/// `build_system_prompt` 的短路是既有契约（用户全权接管提示词，逐字保留），代价是
/// skill 目录段**不会**注入 ⇒ 模型无法自主发现 skill。但不该**静默**：`/skill-name`
/// 的确定性路线仍可用（设计 §F「自定义 prompt 短路」行）。
fn custom_prompt_note(cwd: &std::path::Path, skill_count: usize) -> Option<String> {
    if skill_count == 0 || !prompt::has_custom_prompt(cwd) {
        return None;
    }
    Some(format!(
        "[yushan] 检测到自定义 SYSTEM.md —— 已发现 {skill_count} 个 skill，但 skill 目录\
         **未**注入系统提示（自定义提示词逐字生效），模型无法自主发现它们；`/skill-name` \
         仍可用（不依赖系统提示）。"
    ))
}

/// 汇总启动期提示为**一条**（多行），**绝不互相覆盖**。
///
/// 四类来源彼此独立（缺 `rg`/`fd`、skill 遮蔽、自定义 prompt、窗口装不下固定开销），
/// 合并成一条是因为三者**去处相同**：TUI 模式下它是 `Outbound::Output` 的第一条
/// （排在对话之前），`-p` / `--json` 下是 stderr 的若干行（**绝不能**混进 stdout）。
///
/// 与 `toolset::merge_hints`（私有、两源）同规：空集返回 `None`，否则 `"\n"` 连接。
fn startup_hints(
    tool_hint: Option<String>,
    skill_diags: &[String],
    custom_prompt: Option<String>,
    fixed_cost: Option<String>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    parts.extend(tool_hint);
    parts.extend(skill_diags.iter().cloned());
    parts.extend(custom_prompt);
    parts.extend(fixed_cost);
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// `-p` / `--json` 的 task 展开：与 TUI 的 `Request::Prompt` 走**同一套**逻辑
/// （`skills::expand_invocation`），三模式行为一致（设计 §关键数据流）。
fn expand_task(task: String, skill_entries: &[skills::SkillEntry]) -> String {
    skills::expand_invocation(&task, skill_entries).unwrap_or(task)
}

/// 抓取落盘文件的保留时长：超过即被启动期清理（**只对 Primary 档**生效）。
///
/// `tools-web` 未把它导出为 pub 常量（设计 §1.8 的常量清单里只有散文），故在此
/// 定义为**调用方的策略值**：7 天 —— 足够跨会话 `grep` 回看，又不会无限堆积。
const FETCHED_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// 启动期清理抓取落盘目录 —— **只对可清理档生效**，best-effort、静默。
///
/// # 为什么必须由 `main` 显式调
///
/// 设计 §1.1 硬约束：[`toolset::build_tools`] 与 [`WebConfig::from_env`] **无副作用到
/// 「清理」为止**。若把清理塞进其中任何一处，`toolset` 的**测试**（它们会调
/// `build_tools`）就会删掉用户真实的 `~/.yushan/fetched` —— 测试越界改用户数据。
///
/// # 只对 `Primary` 档
///
/// 用户用 `YUSHAN_FETCHED_DIR` 显式指定的目录（`Override` 档）**只写不删** ——
/// 用户既然点名了目录，清理就是越界。[`FetchedDir::is_cleanable`] 在**类型上**
/// 给出这条保证，不靠自觉。
///
/// 失败静默（照 `logging.rs` 的 best-effort 风格）：清理是**附带动作**，
/// 它永远不该成为一次启动失败的原因。
///
/// # 为什么 `max_age` 是参数而不是直接读常量
///
/// 常量 [`FETCHED_MAX_AGE`]（7 天）是**调用方策略**，生产路径照常传它。把它做成
/// 参数**只为可测**：测试要造「已超龄」的文件，若阈值写死 7 天就只能靠改 mtime
/// 到 8 天前 —— 能测，但测的是「7 天这个数」。参数化后可直接传 `Duration::ZERO`
/// 或小阈值，从而把被测对象收敛到「**是否清理**」（guard 语义），与本函数无关的
/// 保留时长解耦。行为不变：`main` 传的仍是 [`FETCHED_MAX_AGE`]。
///
/// [`FetchedDir::is_cleanable`]: ys_tools_web::FetchedDir::is_cleanable
fn sweep_fetched(web: &WebConfig, max_age: Duration) {
    let fetched = web.fetched();
    if !fetched.is_cleanable() {
        return;
    }
    let Some(dir) = fetched.path() else {
        return;
    };
    let _ = ys_tools_web::sweep(dir, max_age);
}

/// 会话落盘目录 —— **自己的降级绝不产生相对路径**。
///
/// # 四档（优先级从高到低）
///
/// 1. `$YUSHAN_SESSIONS_DIR`（非空）—— 显式覆盖（测试 / 用户指定）。
/// 2. `<HOME>/.yushan/sessions` —— 主目录档。
/// 3. `<USERPROFILE>/.yushan/sessions` —— 同档的 Windows 变量名。
/// 4. `<temp_dir>/yushan-sessions-<pid>` —— 无主目录时的降级。
///
/// # 为什么第 4 档不能是 `.yushan/sessions`
///
/// 相对路径会解析到 **CWD**，而 coding agent 的 CWD **就是用户仓库** —— 会话文件
/// 会落进用户的 git 工作区（`.gitignore` 里有 `tmp/` 时连 `git status` 都看不见，
/// 比看得见的污染更危险）。故降级用 `temp_dir()`，与 `tools-web` 的落盘目录降级
/// 同思路。
///
/// 注意：第 1 档**逐字返回**用户给的值（可能是相对路径）—— 那是显式配置，
/// 责任在配置方；本函数只保证**自己的降级**不产生相对路径。第 2/3 档同样带
/// 绝对性闸门（`HOME=""` / 相对取值会拼出相对路径）。
fn sessions_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("YUSHAN_SESSIONS_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let home = PathBuf::from(home);
        if home.is_absolute() {
            return home.join(".yushan").join("sessions");
        }
    }
    // `TMPDIR` 被设成相对路径时 `temp_dir()` 会**原样返回**它 —— 那时需要一个
    // 恒为绝对的兜底（本项目只支持 Linux/macOS）。
    let base = std::env::temp_dir();
    let base = if base.is_absolute() {
        base
    } else {
        PathBuf::from("/tmp")
    };
    base.join(format!("yushan-sessions-{}", std::process::id()))
}

/// print / json 模式的前置检查：无 model 时给出可操作提示并退出。
///
/// **启动期诊断 → stderr**（设计 §5）：此刻 TUI 还没起屏，stderr 是正确去处。
fn ensure_configured(wiring: &Wiring) -> Result<(), Box<dyn std::error::Error>> {
    if wiring.is_configured() {
        return Ok(());
    }
    eprintln!("Error: No model configured.");
    eprintln!("Set environment variables:");
    eprintln!("  YUSHAN_API_BASE  — API endpoint URL");
    eprintln!("  YUSHAN_API_KEY   — API authentication key");
    eprintln!("  YUSHAN_MODEL     — Model name (optional, default: deepseek-chat)");
    eprintln!("Or run in interactive mode and use /login to configure.");
    std::process::exit(1);
}

/// `-p` / `--json` 的驱动：跑**一个** turn。
///
/// `Inbox` / `Agent::run` 已删（T1）：单次运行就是一次 `run_turn`，`begin_turn(1)`
/// 让 `Envelope.turn` 从 1 起（不再有逐回合递增的多回合循环）。
/// `boundary` 传 `None` —— 一次性模式没有中途插话/中止的 UI 入口。
async fn run_single_turn(
    agent: &mut Agent,
    wiring: &mut Wiring,
    input: AgentInput,
) -> Result<RunResult, LoopError> {
    let ports = wiring.ports();
    ports.events.begin_turn(1);
    agent.run_turn(input, ports).await
}

/// `-p` 模式的事件消费：只把 [`AgentEvent::ModelTextDelta`] 的文本增量写入 `w`
/// （**不换行、按序拼接**），其余事件不产出任何输出；返回是否写出过文本。
///
/// 写出目标参数化为 `W: Write`：生产传 `io::stdout()`，测试传 `Vec<u8>`，
/// 保证被测的就是生产消费路径（`consume_events` + `print_delta_text`）。
/// 终止由 `consume_events` 依**终局事件**判定，不依赖信道关闭。
async fn consume_print_events<W: Write>(rx: mpsc::Receiver<Envelope>, w: W) -> io::Result<bool> {
    let mut printed_any = false;
    consume_events(rx, w, |w, env| {
        if let Some(text) = print_delta_text(env) {
            w.write_all(text.as_bytes())?;
            printed_any = true;
        }
        Ok(())
    })
    .await?;
    Ok(printed_any)
}

/// 把背压读数摘要写到 **stderr**（固定不污染 stdout 的 JSON / 文本流）。
///
/// 仅在「有事发生」（有背压 / 有积压 / 消费者消失）或 `--stats` 显式要求时
/// 输出一行；否则保持 stderr 干净，正常运行时零噪音。
fn report_channel_stats(handle: &ChannelStatsHandle, force: bool) {
    let snapshot = handle.snapshot();
    let line = if force {
        Some(channel::format_stats_forced(&snapshot))
    } else {
        channel::format_stats(&snapshot)
    };
    if let Some(line) = line {
        eprintln!("{line}");
    }
}

/// `-p` 模式：并发消费事件流，把文本增量实时写到 stdout。
///
/// 收尾补一个换行，与旧实现（`println!` final_message）的输出保持一致。
/// turn 结束、消费排空后，把背压读数写到 stderr（见 [`report_channel_stats`]）。
async fn run_print_mode(
    agent: &mut Agent,
    wiring: &mut Wiring,
    rx: mpsc::Receiver<Envelope>,
    task: String,
    stats: &ChannelStatsHandle,
    force_stats: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    ensure_configured(wiring)?;

    // 消费与跑回合必须**并发**：有界信道若无并发消费者，agent 撞满即等待 → 死锁。
    let (run_result, consume_result) = tokio::join!(
        run_single_turn(agent, wiring, AgentInput::text(task.as_str())),
        consume_print_events(rx, io::stdout()),
    );

    // 消费已排空（join 返回）→ 此刻读读数为终态。先报告，再传播错误。
    report_channel_stats(stats, force_stats);
    run_result?;
    let printed_any = consume_result?;

    if printed_any {
        println!();
    }
    Ok(())
}

/// `--json` 模式：并发消费事件流，逐行输出 `Envelope` 的 JSON。
///
/// 背压读数走 stderr（[`report_channel_stats`]），不混入 stdout 的 JSON 流。
async fn run_json_mode(
    agent: &mut Agent,
    wiring: &mut Wiring,
    rx: mpsc::Receiver<Envelope>,
    task: String,
    stats: &ChannelStatsHandle,
    force_stats: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    ensure_configured(wiring)?;

    let (run_result, consume_result) = tokio::join!(
        run_single_turn(agent, wiring, AgentInput::text(task.as_str())),
        consume_events(rx, io::stdout(), write_json_envelope),
    );

    report_channel_stats(stats, force_stats);
    run_result?;
    consume_result?;
    Ok(())
}

/// TUI 模式的全部组装（设计 §7 初始化顺序 1–5）。
///
/// **必须在非 async 线程上调用末尾的 UI 循环** —— 故这里只做「异步初始化 +
/// spawn app 线程 + 跑 UI」这三件事，`rt` 由调用方持有。
///
/// `limits` 是**启动时初值**（`main` 按 provider 恢复后的 model 算好）：挂到 agent
/// 上之后由 `app_loop` 每回合覆盖 `context_window`。
#[allow(clippy::too_many_arguments)]
fn run_interactive(
    rt: tokio::runtime::Runtime,
    mut config: config::Config,
    state_store: state::StateStore,
    system_prompt: String,
    workspace: PathBuf,
    tools: Vec<Box<dyn Tool>>,
    limits: ys_component::RunLimits,
    startup_hint: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. 事件出口：有界信道 + 既定 policy（R7 的阻塞早退不影响它 —— 没 turn 就没有事件）。
    let (sink, sink_rx) =
        ChannelSink::new(channel_capacity(), LifecyclePolicy::StopWhenConsumerGone);

    // 1（续）. agent（挂工具与系统提示；session/model/events 经端口传入）。
    let agent = build_agent(system_prompt, workspace, tools, limits)?;

    // 2.【阻塞】会话 + model 构造 —— 失败会提前返回（此刻 turn 恒 0 也无所谓）。
    let model = config.build_model();
    let wiring = rt.block_on(Wiring::persistent(model, Box::new(sink), sessions_dir()))?;

    // 2b.【阻塞】拉每个已登录 provider 的 `/v1/models`，缓存供 `/model` 浮层用。
    //
    // 必须在**起屏之前**（此后一次重绘都不该发 HTTP，设计 M4）；5s 超时且并发，
    // 失败静默回退静态表。提示**拉完即擦除** —— 它只为解释「这几秒在等什么」，
    // 不该像降级提示那样留在屏幕上。
    let pending = config.registry.logged_in_providers().len();
    if pending > 0 {
        eprint!("Fetching models for {pending} logged-in provider(s)…");
        let _ = io::stderr().flush();
        rt.block_on(config.registry.refresh_models());
        eprint!("\r\x1b[2K");
        let _ = io::stderr().flush();
    }

    let stats = status::TurnStats::default();
    let session_started = Instant::now();

    // 3. view₀ —— 在把 config 移交给 app 线程之前构造。
    let view0 = view::build_view(
        &config,
        &agent,
        &wiring,
        &config.registry,
        &state_store,
        &stats,
        session_started,
    );

    // 4. 三条信道：① Request（回合边界）/ ② Boundary（轮边界）/ ③ Outbound（app → UI）。
    let (request_tx, request_rx) = mpsc::channel::<Request>(REQUEST_CAPACITY);
    let (boundary_tx, boundary_rx) = mpsc::channel::<Boundary>(BOUNDARY_CAPACITY);
    let (out_tx, out_rx) = mpsc::channel::<Outbound<CodingView>>(OUTBOUND_CAPACITY);

    // 4b. 启动期降级提示 → transcript（**不是 stderr**）。
    //
    // 起屏前写 stderr 的行会留在主屏缓冲区里，退出 alt-screen 后又回到屏幕上，
    // 成为残渣（设计 §5 的「命令期间 → `Outbound::Output`」在启动期的延伸）。
    // 在 spawn 之前投递 ⇒ 它是信道里的第一条，稳稳排在对话之前。
    // 容量 1024 且信道全新 ⇒ `try_send` 不可能失败；真失败也只落日志，不 panic。
    if let Some(hint) = startup_hint
        && let Err(e) = out_tx.try_send(Outbound::Output(hint))
    {
        logging::log(&format!("failed to enqueue startup hint: {e}"));
    }

    // 5. app 线程（tokio worker）跑循环；当前线程跑**阻塞**的 UI 循环。
    //    app 循环唯一的失败是「UI 侧收摊」，不该惊动用户 —— 落日志即可。
    rt.spawn(async move {
        if let Err(e) = app_loop::run(
            agent,
            wiring,
            config,
            state_store,
            stats,
            session_started,
            request_rx,
            boundary_rx,
            out_tx,
            sink_rx,
        )
        .await
        {
            logging::log(&format!("app loop exited with error: {e}"));
        }
    });

    // 不在 runtime 上下文里 → `blocking_send` 不会 panic。
    let tui_result = ys_tui_coding::run(view0, out_rx, request_tx, boundary_tx);

    // UI 退出已把 `request_tx` / `boundary_tx` drop → app 线程 `recv()` 得 `None`。
    // 给一段宽限让它收摊（回合跑在半途时最多等这么久），超时连 runtime 一起丢。
    rt.shutdown_timeout(APP_THREAD_GRACE);

    tui_result?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = config::Config::from_env()?;

    // 从 auth.json 加载已保存的凭证
    config.registry.load_auth();

    // 读 `~/.yushan/config.json` 的用户覆盖（context_window / compaction 比例）。
    //
    // 路径由调用方解析（`Config::default_user_config_path`）后显式传入 —— 取不到
    // 路径（无 `HOME` / `USERPROFILE`）就跳过，`load_user_config` 对「文件不存在」
    // 本就静默（用户没写配置文件是常态，不是故障）。
    if let Some(path) = config::Config::default_user_config_path() {
        config.load_user_config(&path);
    }

    // 启动期扫描 skill（**紧跟用户配置**，设计 §关键数据流）：根顺序 = 优先级
    // （project 在前），由 `default_skill_roots` 唯一给出。诊断**不由本模块打印** ——
    // 它与 `toolset` 的降级提示合并成一条，走既有的启动期提示通道。
    let skill_roots = config::Config::default_skill_roots(&config.cwd);
    let skill_diags = config.load_skills(&skill_roots);

    // 目录归一（去重 project > user + 按 command 升序 + 预算收束）一次，供
    // 系统提示段与 `/` 浮层共用；`omitted` 写回 config（`build_view` 的取数口）。
    let catalogue = skills::catalogue(config.skills.clone(), skills::Budget::default());
    config.skills_omitted = catalogue.omitted;

    // 从 state.json 加载已保存的会话状态
    let state_store = state::StateStore::new();
    let saved_state = state_store.load();

    // 启动恢复：若环境变量未提供完整凭证，
    // 先尝试从 state.json（last-active provider+model）恢复，
    // 再回退到第一个存有 auth 的 provider。
    if !config.is_configured() {
        // 尝试 state.json 恢复：last_active_provider + 匹配的 auth entry
        let recovered = saved_state
            .last_active_provider
            .as_deref()
            .and_then(|name| config.registry.find_provider(name))
            .zip(
                saved_state
                    .last_active_provider
                    .as_deref()
                    .and_then(|name| config.registry.auth_for(name)),
            );

        if let Some((provider, entry)) = recovered {
            config.api_base = Some(entry.api_base.clone());
            config.api_key = Some(entry.api_key.clone());
            config.model = saved_state
                .last_active_model
                .clone()
                .unwrap_or_else(|| entry.model.clone());
            config.provider = Some(provider.name.clone());
        } else {
            // 回退：第一个存有凭证的 provider
            for provider in config.registry.providers() {
                if let Some(entry) = config.registry.auth_for(&provider.name) {
                    config.api_base = Some(entry.api_base.clone());
                    config.api_key = Some(entry.api_key.clone());
                    config.model = entry.model.clone();
                    config.provider = Some(provider.name.clone());
                    break;
                }
            }
        }
    }

    // 窗口配置接线（U6b）—— 必须在 **provider 恢复之后**：`context_window()` 按
    // `config.model` 查表，恢复逻辑会改写它。
    //
    // 装配本身在 [`resolve_limits`]（抽出去只为可测，见该函数）；这里算的是
    // **启动时初值**：`-p`/`--json` 单次运行内不会换 provider，一次定死即可；
    // TUI 模式的窗口由 `app_loop` 每回合按当前 model 重算（见该文件）。
    //
    // `mut` 是因为 `fixed_prompt_tokens` 只能在 system prompt 派生出来**之后**量
    // （见下方 §压缩耦合修正）—— 本值不影响 `context_window` / `compaction`。
    let mut limits = resolve_limits(&config);

    // 注册 model factory（适配器特有的构建逻辑）
    config.set_model_factory(|cfg| {
        let base = cfg.api_base.as_ref()?;
        let key = cfg.api_key.as_ref()?;
        let compat = cfg.current_compat();
        Some(Box::new(OpenAICompatibleModel::new(
            OpenAICompatibleConfig {
                api_base: base.clone(),
                api_key: key.clone(),
                model: cfg.model.clone(),
                max_tokens: Some(4096),
                temperature: Some(0.7),
                compat,
            },
        )))
    });

    // 模式必须在建 agent 之前确定：它决定事件消费者与 sink 的选择。
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = parse_args(&argv)?;

    // 构建工具表与 system prompt。
    //
    // 顺序即设计 §1.2「启动期」：**先探测装工具**（`toolset` 是唯一装配接缝，
    // 探测只做一次），降级提示此刻打（TUI 未起屏 → stderr 是正确去处，见
    // 「输出与诊断三分」）；**再由工具表派生提示词的工具清单** —— 同一份表，
    // 清单顺序 == 注册顺序，不可能漂移。
    let cwd = config.cwd.clone();
    let workspace = cwd.clone();
    // web 配置与模型配置**无关**（工具拿不到 `Model` 的正面利用）：`from_env()` 只读
    // env 一次并冻结，**无副作用到「清理」为止**。
    let web = WebConfig::from_env();
    // 清理在此**显式**发生（不在 `build_tools` / `from_env` 里）：只对 Primary 档，
    // 且必须在把 `web` move 进 `build_tools` 之前完成。
    sweep_fetched(&web, FETCHED_MAX_AGE);
    let (tools, hint) = toolset::build_tools(&workspace, web);
    let specs = tools.iter().map(|t| t.spec()).collect::<Vec<_>>();
    let system_prompt = prompt::build_system_prompt(&cwd, &specs, &catalogue);

    // **压缩耦合修正**（设计 §选择理由 E）：压缩触发只看 session 消息，system prompt
    // 与工具表这**每轮固定开销**此前完全在盲区（skill 目录入场后 ~9.6k）。此处量一次
    // 交给 `BasicLoop`，由它加在比较的**输入端**（`trigger_at` 的语义原样不动）。
    limits.fixed_prompt_tokens = fixed_prompt_tokens(&system_prompt, &specs);

    // **启动期提示合并成一条**（多行、绝不互相覆盖）：工具降级 + skill 诊断
    // （遮蔽 / 非法值）+ 自定义 prompt 的代价说明 + R5 的窗口警告。
    // 去处由模式决定：TUI → `Outbound::Output`（transcript）；`-p` / `--json` → stderr。
    let hint = startup_hints(
        hint,
        &skill_diags,
        custom_prompt_note(&cwd, config.skills.len()),
        fixed_cost_note(&limits),
    );

    // 手工建 runtime（而非 `#[tokio::main]`）：TUI 模式的 UI 循环**必须在
    // 没有 runtime 上下文的线程上跑**（它用 `blocking_send`）。
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    match args.mode {
        Mode::Print(task) => {
            // 无 TUI → 启动期降级提示照旧走 stderr（设计 §5 输出三分）。
            if let Some(hint) = &hint {
                eprintln!("{hint}");
            }
            // 与 TUI 的 `Request::Prompt` **同一套**展开：`-p "/git-status"` 也生效。
            let task = expand_task(task, &config.skills);
            let (sink, rx) =
                ChannelSink::new(channel_capacity(), LifecyclePolicy::StopWhenConsumerGone);
            // 装箱前取读数句柄：装箱后仍能读到终态背压计数。
            let stats_handle = sink.stats_handle();
            let mut agent = build_agent(system_prompt, workspace, tools, limits)?;
            // `-p` 是一次性会话：MemorySession，不落盘、不恢复（行为与改动前一致）。
            let mut wiring = Wiring::ephemeral(config.build_model(), Box::new(sink));
            rt.block_on(run_print_mode(
                &mut agent,
                &mut wiring,
                rx,
                task,
                &stats_handle,
                args.stats,
            ))?;
        }
        Mode::Json(task) => {
            // 同 `-p`：无 TUI → stderr（**绝不能**混进 stdout 的逐行 Envelope）。
            if let Some(hint) = &hint {
                eprintln!("{hint}");
            }
            let task = expand_task(task, &config.skills);
            let (sink, rx) =
                ChannelSink::new(channel_capacity(), LifecyclePolicy::StopWhenConsumerGone);
            let stats_handle = sink.stats_handle();
            let mut agent = build_agent(system_prompt, workspace, tools, limits)?;
            let mut wiring = Wiring::ephemeral(config.build_model(), Box::new(sink));
            rt.block_on(run_json_mode(
                &mut agent,
                &mut wiring,
                rx,
                task,
                &stats_handle,
                args.stats,
            ))?;
        }
        Mode::Interactive => {
            // TUI 模式下提示**不进 stderr**，改走 transcript（`Outbound::Output`）——
            // 起屏前写 stderr 的行会落在主屏上，退出 alt-screen 后成为残渣。
            run_interactive(
                rt,
                config,
                state_store,
                system_prompt,
                workspace,
                tools,
                limits,
                hint,
            )?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};

    use ys_core::{ContentBlock, Message, Role, StopReason, ToolCall, ToolCallId, Usage};
    use ys_model::MockModel;
    use ys_protocol::Source;
    use ys_tools_web::FetchedDir;

    use crate::test_env::{EnvRestore, env_lock};

    /// 进程内唯一临时目录（**原子序号 + `process::id()`**，与 `toolset.rs` 同规）：
    /// `SystemTime::as_nanos()` 在 macOS 只到微秒级，并行测试会撞名互删。
    fn unique_tmp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("yushan_sweep_{tag}_{}_{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 在 `dir` 下写一个**超龄**（mtime = 8 天前）且名字**符合落盘命名形状**
    /// （`<slug>-<16hex>.md`）的文件，返回其路径。
    ///
    /// 名字直接调 `ys_tools_web::fetched_file_name`（落盘文件名的**唯一定义**），
    /// 不手抄形状 —— 否则 `sweep` 认不出它，测试会「因为文件本来就不会被删」而假绿。
    /// mtime 用 `File::set_modified`（Rust 1.75+ 稳定）显式拨到 8 天前，不依赖
    /// 文件系统时间戳精度或写入耗时，从而**确定性**地满足 `mtime < now - max_age`。
    fn write_aged_fetched(dir: &Path, final_url: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(ys_tools_web::fetched_file_name(final_url));
        let file = std::fs::File::create(&path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60))
            .unwrap();
        path
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// 测试用默认 limits —— `build_agent` 现在必须显式收一个（U6b 接线）。
    fn default_limits() -> ys_component::RunLimits {
        ys_component::RunLimits::default()
    }

    fn env(event: AgentEvent) -> Envelope {
        Envelope::new(Source::agent(), 0, event)
    }

    fn delta(text: &str) -> AgentEvent {
        AgentEvent::ModelTextDelta {
            text: text.to_string(),
        }
    }

    fn finished() -> AgentEvent {
        AgentEvent::RunFinished {
            stop_reason: StopReason::Completed,
            usage: Usage::default(),
            rounds: 1,
        }
    }

    /// 记录每次 `write` 调用的 writer，用于断言「分次写出」而非「缓冲后一次写出」。
    ///
    /// `flush` 不计入 `write_calls`（`consume_events` 每事件后都会 flush，
    /// 若计入则无法区分写出次数与冲刷次数）。
    #[derive(Default)]
    struct CountingWriter {
        bytes: Vec<u8>,
        write_calls: usize,
    }

    impl Write for CountingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.write_calls += 1;
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// 1. `-p <任务>` → Print。
    #[test]
    fn parse_p_single_arg() {
        assert_eq!(
            parse_args(&s(&["-p", "hello"])).unwrap(),
            Args {
                mode: Mode::Print("hello".into()),
                stats: false,
            }
        );
    }

    /// 2. `--json <任务...>` → Json，多参数 join。
    #[test]
    fn parse_json_joins_remaining_args() {
        assert_eq!(
            parse_args(&s(&["--json", "do", "x"])).unwrap(),
            Args {
                mode: Mode::Json("do x".into()),
                stats: false,
            }
        );
    }

    /// 3. 无 flag → Interactive。
    #[test]
    fn parse_no_flag_is_interactive() {
        assert_eq!(
            parse_args(&[]).unwrap(),
            Args {
                mode: Mode::Interactive,
                stats: false,
            }
        );
    }

    /// 4. `-p` 后无参数 → Err。
    #[test]
    fn parse_p_without_task_errors() {
        let err = parse_args(&s(&["-p"])).unwrap_err();
        assert!(err.contains("-p"), "err = {err}");
    }

    /// 5. `--json` 后无参数 → Err。
    #[test]
    fn parse_json_without_task_errors() {
        let err = parse_args(&s(&["--json"])).unwrap_err();
        assert!(err.contains("--json"), "err = {err}");
    }

    /// 补充：未知 flag（首位置）→ Err；但任务文本里的 `-` 开头不受影响。
    #[test]
    fn parse_unknown_flag_errors_but_task_text_is_verbatim() {
        assert!(parse_args(&s(&["--nope"])).is_err());
        // 任务文本以 `-` 开头（非首位置）仍原样保留
        assert_eq!(
            parse_args(&s(&["-p", "--help", "me"])).unwrap().mode,
            Mode::Print("--help me".into())
        );
    }

    /// `--stats` 紧随模式 flag → 识别为 flag，任务文本不含它；两种模式均可组合。
    #[test]
    fn parse_stats_flag_after_mode() {
        assert_eq!(
            parse_args(&s(&["-p", "--stats", "hi"])).unwrap(),
            Args {
                mode: Mode::Print("hi".into()),
                stats: true,
            }
        );
        assert_eq!(
            parse_args(&s(&["--json", "--stats", "do", "x"])).unwrap(),
            Args {
                mode: Mode::Json("do x".into()),
                stats: true,
            }
        );
    }

    /// `--stats` 只认「模式 flag 之后、任务文本之前」：任务文本一旦开始（含
    /// 首参数被 `--stats` 后仍有文本），后续 `--stats` 就是文本的一部分。
    #[test]
    fn parse_stats_after_task_text_is_verbatim() {
        assert_eq!(
            parse_args(&s(&["-p", "hi", "--stats"])).unwrap(),
            Args {
                mode: Mode::Print("hi --stats".into()),
                stats: false,
            }
        );
        // 既有语义不破：`--help` 从来不是 flag（仅首位置判定，且此处非 `--stats`）。
        assert_eq!(
            parse_args(&s(&["-p", "--help", "me"])).unwrap(),
            Args {
                mode: Mode::Print("--help me".into()),
                stats: false,
            }
        );
    }

    /// `-p --stats`（有 flag 无任务）→ Err，不会把 `--stats` 当任务。
    #[test]
    fn parse_stats_without_task_errors() {
        let err = parse_args(&s(&["-p", "--stats"])).unwrap_err();
        assert!(err.contains("-p"), "err = {err}");
        let err = parse_args(&s(&["--json", "--stats"])).unwrap_err();
        assert!(err.contains("--json"), "err = {err}");
    }

    /// 6. `write_json_envelope`：合法 JSON、含 `event` 字段、以 `\n` 结尾；
    ///    连续两条 → 两行。
    #[test]
    fn write_json_envelope_emits_one_json_line() {
        let e = env(delta("hi"));
        let mut buf = Vec::new();
        write_json_envelope(&mut buf, &e).unwrap();

        assert!(buf.ends_with(b"\n"), "应以换行结尾");
        let line = std::str::from_utf8(&buf).unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert!(v.get("event").is_some(), "json = {line}");
        assert_eq!(v["source"], "agent");
        assert_eq!(v["turn"], 0);
        assert_eq!(v["event"]["ModelTextDelta"]["text"], "hi");

        let mut buf2 = Vec::new();
        write_json_envelope(&mut buf2, &e).unwrap();
        write_json_envelope(&mut buf2, &e).unwrap();
        let text = String::from_utf8(buf2).unwrap();
        assert_eq!(text.lines().count(), 2, "两条信封应为两行");
    }

    /// 7. `print_delta_text`：ModelTextDelta → Some；ToolCall / RunFinished → None。
    #[test]
    fn print_delta_text_only_matches_model_text_delta() {
        let d = env(delta("chunk"));
        assert_eq!(print_delta_text(&d), Some("chunk"));

        let tool_call = env(AgentEvent::ToolCall {
            call: ToolCall {
                id: ToolCallId("c1".into()),
                name: "bash".into(),
                arguments: serde_json::Value::Null,
            },
        });
        assert_eq!(print_delta_text(&tool_call), None);
        assert_eq!(print_delta_text(&env(finished())), None);
    }

    /// 8. **防死锁关键测试**：终局事件到达即返回，**不**等信道关闭。
    ///    sender 保持不 drop —— 若实现退化为「等 rx.recv() 返回 None」，此测试会超时。
    #[tokio::test]
    async fn consume_events_stops_at_terminal_event_without_channel_close() {
        let (tx, rx) = mpsc::channel(8);
        tx.send(env(delta("a"))).await.unwrap();
        tx.send(env(delta("b"))).await.unwrap();
        tx.send(env(finished())).await.unwrap();

        let mut out: Vec<u8> = Vec::new();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            consume_events(rx, &mut out, write_json_envelope),
        )
        .await;

        assert!(result.is_ok(), "应收到终局事件即返回，而非等待信道关闭");
        result.unwrap().unwrap();

        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 3, "三条信封应全部写出");
        assert!(text.contains("RunFinished"), "text = {text}");

        // 显式保留到断言之后：证明 consume_events 未依赖 tx 被 drop。
        drop(tx);
    }

    /// 9. **M2 并发死锁回归**：小容量（2）下 `run_turn` 与 `consume_events`
    ///    并发执行，MockModel 产出的事件数 > 容量。
    ///
    ///    旧实现下 run_turn 内的自由函数先走同步快路径，不 yield；
    ///    终局事件被缓冲进 overflow 后无人冲刷 → consume 永不返回 → join 挂起。
    ///    修复后（自由函数总走慢路径）应在超时内完成且收到 RunFinished。
    ///
    ///    驱动走**生产路径** [`run_single_turn`]（`begin_turn(1)` + `run_turn`）。
    #[tokio::test]
    async fn run_turn_and_consume_do_not_deadlock_at_small_capacity() {
        let model = MockModel::new("m");
        model.push_text("hello world");

        let (sink, rx) = ChannelSink::new(2, LifecyclePolicy::StopWhenConsumerGone);
        let mut agent = build_agent(
            String::new(),
            PathBuf::from("."),
            Vec::new(),
            default_limits(),
        )
        .unwrap();
        let mut wiring = Wiring::ephemeral(Some(Box::new(model)), Box::new(sink));

        let mut out: Vec<u8> = Vec::new();
        let joined = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                run_single_turn(&mut agent, &mut wiring, AgentInput::text("hi")),
                consume_events(rx, &mut out, write_json_envelope),
            )
        })
        .await;

        assert!(joined.is_ok(), "run_turn 与 consume_events 并发不得挂起");
        let (turn_result, consume_result) = joined.unwrap();
        turn_result.expect("turn 应成功");
        consume_result.expect("consume 应成功");

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("RunFinished"), "text = {text}");
        assert!(text.contains("ModelTextDelta"), "text = {text}");
    }

    /// 10. 容量 clamp：过小容量被抬到下限；正常容量不动。
    #[test]
    fn channel_capacity_is_clamped_to_floor() {
        assert_eq!(clamp_capacity(1), MIN_CHANNEL_CAPACITY);
        assert_eq!(clamp_capacity(2), MIN_CHANNEL_CAPACITY);
        assert_eq!(
            clamp_capacity(MIN_CHANNEL_CAPACITY - 1),
            MIN_CHANNEL_CAPACITY
        );
        assert_eq!(clamp_capacity(MIN_CHANNEL_CAPACITY), MIN_CHANNEL_CAPACITY);
        assert_eq!(
            clamp_capacity(DEFAULT_CHANNEL_CAPACITY),
            DEFAULT_CHANNEL_CAPACITY
        );
    }

    /// 12. **-p 按序增量拼接（Task 14 核心不变量）**：多个 `ModelTextDelta` 必须**按序、
    ///     分次**拼接到写出目标；非文本事件（`UserMessage` / `ToolCall`）不产出；
    ///     终局事件（`RunFinished`）不产出且触发**正常返回**（不依赖 sender drop）。
    ///     拼接结果恰为 `"Hello world"` —— 锁定「无丢字、无重复、无多余换行」。
    ///
    ///     **「增量」语义单独锁定**：用 [`CountingWriter`] 断言 `write_calls >= 3`
    ///     （每个 delta 至少一次 write）。若实现退化为「全部缓冲、收到终局事件后
    ///     一次性写出」，内容仍为 `"Hello world"` 但 `write_calls == 1`，此断言失败。
    ///
    ///     说明：模型适配器已接通流式（`stream: true`），生产中一次响应会发多个
    ///     delta，本路径即为其真实消费路径；本测试**手工投递多个 delta**锁定顺序
    ///     与增量写出语义。消费走**生产路径** `consume_print_events`
    ///     （内部即 `consume_events` + `print_delta_text`），未重写逻辑。
    #[tokio::test]
    async fn print_mode_writes_deltas_in_order() {
        let (tx, rx) = mpsc::channel(8);
        tx.send(env(AgentEvent::UserMessage {
            message: Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
            },
        }))
        .await
        .unwrap();
        tx.send(env(delta("Hel"))).await.unwrap();
        tx.send(env(delta("lo"))).await.unwrap();
        tx.send(env(delta(" world"))).await.unwrap();
        tx.send(env(AgentEvent::ToolCall {
            call: ToolCall {
                id: ToolCallId("c1".into()),
                name: "bash".into(),
                arguments: serde_json::Value::Null,
            },
        }))
        .await
        .unwrap();
        tx.send(env(finished())).await.unwrap();

        let mut out = CountingWriter::default();
        let result =
            tokio::time::timeout(Duration::from_secs(5), consume_print_events(rx, &mut out)).await;

        assert!(
            result.is_ok(),
            "收到 RunFinished 应即返回，而非等待信道关闭"
        );
        let printed_any = result.unwrap().unwrap();
        assert!(printed_any, "应写出过文本增量");

        // 顺序正确、无非文本事件输出、无多余换行：恰为 "Hello world"。
        assert_eq!(out.bytes.len(), "Hello world".len(), "字节数应恰为 11");
        assert_eq!(String::from_utf8(out.bytes).unwrap(), "Hello world");

        // **增量写出**：3 个 delta 各写一次，而非缓冲后一次写出。
        // 用 `>= 3`（而非 `== 3`）容忍 `write_all` 内部可能的合并写出。
        assert!(
            out.write_calls >= 3,
            "每个 delta 至少一次 write，证明是增量写出而非缓冲后一次写出；write_calls = {}",
            out.write_calls
        );

        // 显式保留到断言之后：证明 consume_print_events 未依赖 tx 被 drop。
        drop(tx);
    }

    /// 13. **一次性模式的回合号**：`-p`/`--json` 走 [`run_single_turn`]
    ///     （`begin_turn(1)` + `run_turn`），信道上所有 `Envelope.turn` 从 **1** 起。
    ///
    ///     `Inbox`/`Agent::run` 已删：不再有逐回合递增，一次运行恒为 turn 1。
    #[tokio::test]
    async fn run_single_turn_sets_turn_from_one() {
        let model = MockModel::new("m");
        model.push_text("hello");

        let (sink, rx) = ChannelSink::new(
            DEFAULT_CHANNEL_CAPACITY,
            LifecyclePolicy::StopWhenConsumerGone,
        );
        let mut agent = build_agent(
            String::new(),
            PathBuf::from("."),
            Vec::new(),
            default_limits(),
        )
        .unwrap();
        let mut wiring = Wiring::ephemeral(Some(Box::new(model)), Box::new(sink));

        let mut out: Vec<u8> = Vec::new();
        let (run_result, consume_result) = tokio::join!(
            run_single_turn(&mut agent, &mut wiring, AgentInput::text("hi")),
            consume_events(rx, &mut out, write_json_envelope),
        );
        run_result.expect("run_turn 应成功");
        consume_result.expect("consume 应成功");

        let text = String::from_utf8(out).unwrap();
        let turns: Vec<u64> = text
            .lines()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["turn"]
                    .as_u64()
                    .expect("Envelope 应含 turn 字段")
            })
            .collect();

        assert!(!turns.is_empty(), "应至少收到一个信封；text = {text}");
        assert!(text.contains("RunFinished"), "text = {text}");
        assert!(
            turns.iter().all(|&t| t == 1),
            "所有信封的 turn 应从 1 开始；turns = {turns:?}"
        );
    }

    /// 14. **`sessions_dir` 回归（S11b）**：无 `HOME` / `USERPROFILE` 且
    ///     `YUSHAN_SESSIONS_DIR` 未设时，降级路径**必须是绝对路径**。
    ///
    ///     改前的 bug：回退到 `PathBuf::from(".yushan/sessions")` —— **相对路径**，
    ///     会解析到 CWD，而 coding agent 的 CWD **就是用户仓库** → 会话文件落进
    ///     用户的 git 工作区（`.gitignore` 有 `tmp/` 时连 `git status` 都看不见）。
    ///
    ///     ⚠️ 必须同时摘掉 `HOME` **与** `USERPROFILE`：任一个存在都会走主目录档，
    ///     那档本身就含 `.yushan/sessions`（但那是**绝对**路径，是合法结果），
    ///     从而让本用例的「不含相对形态」断言失去意义。
    ///
    ///     变异：把回退改回 `PathBuf::from(".yushan/sessions")` → 本用例变红
    ///     （`is_absolute()` 与「不含 `.yushan/sessions`」两条同时失败）。
    #[test]
    fn sessions_dir_fallback_is_absolute_when_home_missing() {
        let _guard = env_lock();
        let _env = EnvRestore::capture(&["HOME", "USERPROFILE", "YUSHAN_SESSIONS_DIR"]);
        // SAFETY: 调用方持有 env_lock，独占这几个 key 的修改。
        unsafe {
            std::env::remove_var("HOME");
            std::env::remove_var("USERPROFILE");
            std::env::remove_var("YUSHAN_SESSIONS_DIR");
        }

        let dir = sessions_dir();
        assert!(
            dir.is_absolute(),
            "无主目录时的降级必须是绝对路径（相对路径会落进用户仓库）；got = {}",
            dir.display()
        );
        assert!(
            !dir.to_string_lossy().contains(".yushan/sessions"),
            "降级不得退化成相对形态 .yushan/sessions；got = {}",
            dir.display()
        );
        assert!(
            dir.to_string_lossy().contains("yushan-sessions-"),
            "降级应是 <temp_dir>/yushan-sessions-<pid>；got = {}",
            dir.display()
        );

        // 主目录存在时仍走主目录档（绝对路径）——确认改动没有把正常路径也换掉。
        // SAFETY: 同上，仍持锁。
        unsafe {
            std::env::set_var("HOME", "/tmp/yushan-home-probe");
        }
        let with_home = sessions_dir();
        assert_eq!(
            with_home,
            PathBuf::from("/tmp/yushan-home-probe/.yushan/sessions"),
            "有 HOME 时应走主目录档"
        );
    }

    /// 15. **`sweep_fetched` 的档位 guard**：只清 `Primary`，`Override` 档**只写不删**。
    ///
    ///     两个临时目录扮演两档，各放一个**超龄且命名形状正确**的文件：
    ///     - `Override` → 文件**必须仍在**（用户点名的目录，清理即越界）；
    ///     - `Primary`  → 文件**必须被删**。
    ///
    ///     第二条是第一条的**对照**：它证明这个探针**能**删（命名形状、mtime、
    ///     目录参数都对得上），否则「Override 文件仍在」可能只是因为**什么都没删**，
    ///     变成一条恒真的空断言。
    ///
    ///     变异：删掉 `sweep_fetched` 里 `if !fetched.is_cleanable() { return; }`
    ///     这段 guard → `Override` 文件被删 → 本用例第一条断言变红。
    #[test]
    fn sweep_fetched_only_cleans_primary_tier() {
        let name_src = "https://example.com/aged-article";

        // ---- 档 3：Override（用户显式指定）→ 只写不删 ----
        let override_dir = unique_tmp_dir("override");
        let override_file = write_aged_fetched(&override_dir, name_src);
        let override_web = WebConfig::offline(FetchedDir::Override(override_dir.clone()));
        // max_age 传 7 天（== 生产常量）：文件 mtime 是 8 天前，确实超龄。
        sweep_fetched(&override_web, FETCHED_MAX_AGE);
        assert!(
            override_file.exists(),
            "Override 档（$YUSHAN_FETCHED_DIR）只写不删 —— 用户点名的目录不得被清理；got = {}",
            override_file.display()
        );

        // ---- 档 2：Primary（应用目录）→ 清理生效 ----
        let primary_dir = unique_tmp_dir("primary");
        let primary_file = write_aged_fetched(&primary_dir, name_src);
        let primary_web = WebConfig::offline(FetchedDir::Primary(primary_dir.clone()));
        sweep_fetched(&primary_web, FETCHED_MAX_AGE);
        assert!(
            !primary_file.exists(),
            "Primary 档的超龄落盘文件应被清理（否则上一条断言是空断言）"
        );

        let _ = std::fs::remove_dir_all(&override_dir);
        let _ = std::fs::remove_dir_all(&primary_dir);
    }

    /// 15b. **「未超龄」不删**：同一 `Primary` 档、同一命名形状，mtime = 现在 +
    ///      `max_age` = 7 天 → 文件必须**仍在**。
    ///
    ///      这是 15 的**阈值对照**：15 只证明「超龄会被删」，若实现把阈值判据写反
    ///      （`mtime >= cutoff` 写成恒真/恒假的另一侧），15 仍可能绿。本用例把
    ///      `mtime > cutoff` 这一侧钉住。
    ///
    ///      变异：`store.rs::sweep` 的 `if mtime >= cutoff { continue; }` 改成
    ///      `if mtime <= cutoff { continue; }` → 本用例变红（刚写的文件被删）。
    #[test]
    fn sweep_fetched_keeps_recent_files() {
        let primary_dir = unique_tmp_dir("recent");
        let fresh_file = write_aged_fetched(&primary_dir, "https://example.com/fresh-article");
        // 把 mtime 拨回「刚刚」——在 7 天阈值内。
        std::fs::File::options()
            .write(true)
            .open(&fresh_file)
            .unwrap()
            .set_modified(SystemTime::now())
            .unwrap();

        let web = WebConfig::offline(FetchedDir::Primary(primary_dir.clone()));
        sweep_fetched(&web, FETCHED_MAX_AGE);
        assert!(
            fresh_file.exists(),
            "未超龄（7 天内）的落盘文件不得被清理；got = {}",
            fresh_file.display()
        );

        let _ = std::fs::remove_dir_all(&primary_dir);
    }

    /// 16. **`build_agent` 必须把 `limits` 真的挂到 agent 上**（U6b 根因回归）。
    ///
    ///     改前 `build_agent` 从不调 `.limits()` ⇒ `AgentBuilder` 落回
    ///     `RunLimits::default()` ⇒ `context_window` 恒为 128_000 —— 这就是
    ///     「1M 模型在 11 万 token 就被压缩」的根因（压缩阈值按错误的窗口算）。
    ///
    ///     变异：删掉 `build_agent` 里的 `.limits(limits)` → 断言变红
    ///     （`128_000 != 1_000_000`）。
    #[test]
    fn build_agent_applies_context_window_from_limits() {
        let mut limits = ys_component::RunLimits::default();
        limits.context_window = 1_000_000;
        limits.compaction.trigger_ratio = 0.5;

        let agent = build_agent(String::new(), PathBuf::from("."), Vec::new(), limits)
            .expect("空工具表也应能构建");

        assert_eq!(
            agent.context_window(),
            1_000_000,
            "传入的 limits.context_window 必须生效，否则压缩按错误的窗口触发"
        );
        // 反证：这个值确实不同于「落回默认」的结果（不是恒真断言）
        assert_ne!(
            agent.context_window(),
            ys_component::RunLimits::default().context_window,
            "若仍等于默认窗口，说明 .limits() 没生效"
        );
    }

    /// **`resolve_limits` 必须真的从 `Config` 取值**（U6b 装配回归）。
    ///
    /// [`build_agent_applies_context_window_from_limits`] 钉住的是「**传入的 limits**
    /// 会生效」；但「**从 config 取出来再传进去**」那一步原先在 `main()` 里，
    /// 测试够不着 —— 若有人把它误写成 `RunLimits::default()`，那条测试照样全绿，
    /// 而本刀修的 bug（1M 模型按 128k 压缩）会**悄悄复活**。本用例直接调
    /// [`resolve_limits`]，把装配这一步钉住。
    ///
    /// 断言**刻意选与默认值不同的数**：窗口 `262_144 ≠ 128_000`、触发比例
    /// `0.5 ≠ 0.85`（`CompactPolicy::default`）。若取默认值，两个值都能区分开，
    /// 断言不会恒真。
    ///
    /// 变异：把 `resolve_limits` 改成直接 `return RunLimits::default()` →
    /// 本用例变红（`context_window` 落回 128_000、`trigger_ratio` 落回 0.85）。
    #[test]
    fn resolve_limits_reads_window_and_policy_from_config() {
        // `Config::from_env` 会 `getenv` 四个凭证 key，而 `config.rs` 的用例正在
        // `env_lock` 下对这些 key 做 `set_var` / `remove_var` —— 二者跨线程并发即
        // 数据竞争（`set_var` 在 edition 2024 因此是 `unsafe`）。**只读也要持锁**，
        // 把本用例与那些改写串行化。
        let _guard = crate::test_env::env_lock();

        let dir = unique_tmp_dir("resolve_limits");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{
                "context_window": { "resolve-limits-model": 262144 },
                "compaction": { "trigger_ratio": 0.5, "keep_ratio": 0.25 }
            }"#,
        )
        .unwrap();

        // `model` 随即被覆盖，环境里的 `YUSHAN_MODEL` 影响不到本用例的断言；
        // 但 `from_env` 的 `getenv` 仍须与改写的测试串行（见函数开头的锁）。
        let mut config = config::Config::from_env().unwrap();
        config.model = "resolve-limits-model".into();
        config.load_user_config(&path);

        let limits = resolve_limits(&config);

        assert_eq!(
            limits.context_window, 262_144,
            "context_window 必须来自 config（config.json 的按 model 覆盖）"
        );
        assert_ne!(
            limits.context_window,
            ys_component::RunLimits::default().context_window,
            "若等于默认 128_000，说明装配没引用 config"
        );
        assert_eq!(
            limits.compaction.trigger_ratio, 0.5,
            "compact_policy 必须来自 config"
        );
        assert_eq!(limits.compaction.keep_ratio, 0.25, "逐字段独立覆盖");
        assert_ne!(
            limits.compaction.trigger_ratio,
            ys_component::CompactPolicy::default().trigger_ratio,
            "若等于默认 0.85，说明装配没引用 config"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ───────────────────── skills 启动装配（T13） ─────────────────────

    /// **`fixed_prompt_tokens` 由 system prompt 与工具表派生**（设计 §选择理由 E）：
    /// 必须 > 0（工具表的 JSON spec 本身就占 token），且随 prompt 增长。
    ///
    /// 变异：把它写死成 `RunLimits::default()` 的 0（= 本刀修的那个盲区复活）⇒
    /// 第一条断言变红。
    #[test]
    fn fixed_prompt_tokens_derives_from_prompt_and_tool_specs() {
        let specs = vec![ys_tool::ToolSpec::new(
            "read",
            "Read file contents",
            serde_json::json!({"type": "object"}),
        )];

        let short = fixed_prompt_tokens("hi", &specs);
        let long = fixed_prompt_tokens(&"x".repeat(4096), &specs);
        assert!(short > 0, "工具表 spec 本身即固定开销");
        assert!(
            long > short,
            "prompt 变长 ⇒ 固定开销必须变长（{short} → {long}）"
        );

        // 边界：空 prompt + 空工具表 ⇒ 0（估算器对空串给 0，`[]` 也只有 2 字节）
        assert_eq!(fixed_prompt_tokens("", &[]), 0);
        assert_eq!(fixed_prompt_tokens("abcdefgh", &[]), 2);
    }

    /// **R5**：固定开销 ≥ 压缩触发点 ⇒ 必须有明确提示；差一个 token 就不提示。
    ///
    /// 两条对照（`fixed = trigger - 1` 与大窗口 + 9.6k 固定开销）是为了让「有提示」
    /// 不成为恒真断言 —— 一个永远返回 `Some` 的实现过不了后两条。
    ///
    /// 变异：把判据写成 `<`（方向倒转）⇒ 第一条断言变红。
    #[test]
    fn fixed_cost_note_fires_when_trigger_point_is_out_of_reach() {
        let mut limits = ys_component::RunLimits::default();
        limits.context_window = 8_000;
        let trigger = limits.compaction.trigger_at(8_000);
        assert!(trigger > 0, "测试前提：触发点非零（P1 不变量给的兜底）");

        limits.fixed_prompt_tokens = trigger;
        let note = fixed_cost_note(&limits).expect("固定开销 = 触发点 ⇒ 必须提示");
        assert!(note.contains(&trigger.to_string()), "提示应给出触发点：{note}");
        assert!(note.contains("context_window"), "提示应可操作：{note}");

        // 差一个 token ⇒ 仍装得下 ⇒ 不提示。
        limits.fixed_prompt_tokens = trigger - 1;
        assert!(
            fixed_cost_note(&limits).is_none(),
            "未达触发点不该打扰用户"
        );

        // 真实量级的对照：128k 窗口装得下 ~9.6k 的 skill 目录。
        let mut wide = ys_component::RunLimits::default();
        wide.fixed_prompt_tokens = 9_600;
        assert!(
            fixed_cost_note(&wide).is_none(),
            "128k 窗口下 skill 目录（~9.6k）远不到触发点"
        );
    }

    /// 启动期提示**合并成一条、绝不互相覆盖**（照 `toolset::merge_hints` 的立场：
    /// 四类来源彼此独立，丢任何一条用户就少了可操作信息）。
    ///
    /// 变异：把实现改成「后取者覆盖先取者」（只留一条）⇒ 各 `contains` 断言变红。
    #[test]
    fn startup_hints_merge_all_sources() {
        let merged = startup_hints(
            Some("tool-hint".into()),
            &["skill-diag-a".to_string(), "skill-diag-b".to_string()],
            Some("custom-note".into()),
            Some("fixed-note".into()),
        )
        .expect("四源皆有 ⇒ 一条提示");

        for part in [
            "tool-hint",
            "skill-diag-a",
            "skill-diag-b",
            "custom-note",
            "fixed-note",
        ] {
            assert!(merged.contains(part), "合并后不得丢掉 `{part}`：{merged}");
        }
        assert!(merged.starts_with("tool-hint"), "工具降级在最前：{merged}");
        assert!(merged.ends_with("fixed-note"), "R5 在最后：{merged}");
        assert_eq!(merged.lines().count(), 5, "一行一事：{merged}");

        // 全空 ⇒ None（不留空行）。
        assert!(startup_hints(None, &[], None, None).is_none());

        // **只有 skill 诊断**也要出（工具侧没有不能把 skill 侧吞掉）。
        assert_eq!(
            startup_hints(None, &["x".into()], None, None).as_deref(),
            Some("x")
        );
    }

    /// 自定义 prompt 提示：**两个条件同时成立**才出（有自定义 SYSTEM.md 且发现了 skill）。
    ///
    /// 变异：去掉 `skill_count == 0` 的守卫 ⇒ 第二条断言变红（没 skill 还提示 skill
    /// 不可见，是纯粹的噪音）。
    #[test]
    fn custom_prompt_note_requires_both_conditions() {
        let _guard = env_lock();
        let _env = EnvRestore::capture(&["HOME", "USERPROFILE"]);
        // SAFETY: 调用方持有 env_lock，独占这两个 key 的修改与恢复。
        unsafe {
            std::env::set_var("HOME", "/nonexistent-yushan-note-home");
            std::env::remove_var("USERPROFILE");
        }

        let dir = unique_tmp_dir("custom_note");
        assert!(
            custom_prompt_note(&dir, 3).is_none(),
            "无自定义 prompt ⇒ 无提示"
        );

        let system_dir = dir.join(".yushan");
        std::fs::create_dir_all(&system_dir).unwrap();
        std::fs::write(system_dir.join("SYSTEM.md"), "Custom").unwrap();

        assert!(
            custom_prompt_note(&dir, 0).is_none(),
            "一个 skill 都没有 ⇒ 无提示（避免噪音）"
        );
        let note = custom_prompt_note(&dir, 3).expect("两者皆有 ⇒ 提示");
        assert!(note.contains('3'), "提示应给出 skill 数：{note}");
        assert!(note.contains("/skill-name"), "提示应指出确定性路线仍可用：{note}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **`-p` / `--json` 的 task 也展开**（三模式行为一致）：命中已知 skill 时 task
    /// 被重写成带正文的信封；普通 task **逐字节不变**。
    ///
    /// 变异：把 `expand_task` 改成恒等（忘了接）⇒ 第一条断言变红。
    #[test]
    fn print_task_skill_invocation_is_expanded_like_the_tui_path() {
        let dir = unique_tmp_dir("expand_task");
        let skill_dir = dir.join("git-status");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: git-status\ndescription: 状态\n---\n正文第一行\n",
        )
        .unwrap();
        let (entries, diags) = crate::skills::scan(&[dir.clone()]);
        assert_eq!(entries.len(), 1, "测试前提：{diags:?}");

        let expanded = expand_task("/git-status 现在改了什么".to_string(), &entries);
        assert!(expanded.contains("<skill name=\"git-status\""), "{expanded}");
        assert!(expanded.contains("正文第一行"), "{expanded}");
        assert!(expanded.contains("现在改了什么"), "参数原样保留：{expanded}");

        let plain = "修一下这个 bug".to_string();
        assert_eq!(
            expand_task(plain.clone(), &entries),
            plain,
            "非 skill 的 task 必须逐字节不变"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
