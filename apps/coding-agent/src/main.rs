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
//! 1. 建 ChannelSink（policy 已定）+ 建 Agent（挂工具与系统提示）
//! 2. 【阻塞】model + Wiring 构造   ← 可能失败并提前返回（turn 恒 0 也无所谓）
//! 3. 构造 view₀
//! 4. 建三条信道 request / boundary / outbound
//! 5. spawn app 线程 + 当前线程跑 UI
//! ```
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
mod state;
mod status;
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
use ys_tools_basic::{BashTool, EditTool, ReadTool, WriteTool};
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
fn build_agent(system_prompt: String, workspace: PathBuf) -> Result<Agent, BuildError> {
    AgentBuilder::new()
        .tool(ReadTool::new(workspace.clone()))
        .tool(WriteTool::new(workspace.clone()))
        .tool(EditTool::new(workspace.clone()))
        .tool(BashTool::new(workspace.clone()))
        .system_prompt(system_prompt)
        .working_dir(workspace.clone(), workspace)
        .approval(ys_tool::AutoApprove)
        .build()
}

/// 会话落盘目录：`$YUSHAN_SESSIONS_DIR` 覆盖，否则 `~/.yushan/sessions`。
fn sessions_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("YUSHAN_SESSIONS_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    home.map(|h| PathBuf::from(h).join(".yushan").join("sessions"))
        .unwrap_or_else(|| PathBuf::from(".yushan/sessions"))
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
#[allow(clippy::too_many_arguments)]
fn run_interactive(
    rt: tokio::runtime::Runtime,
    config: config::Config,
    state_store: state::StateStore,
    system_prompt: String,
    workspace: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. 事件出口：有界信道 + 既定 policy（R7 的阻塞早退不影响它 —— 没 turn 就没有事件）。
    let (sink, sink_rx) =
        ChannelSink::new(channel_capacity(), LifecyclePolicy::StopWhenConsumerGone);

    // 1（续）. agent（挂工具与系统提示；session/model/events 经端口传入）。
    let agent = build_agent(system_prompt, workspace)?;

    // 2.【阻塞】会话 + model 构造 —— 失败会提前返回（此刻 turn 恒 0 也无所谓）。
    let model = config.build_model();
    let wiring = rt.block_on(Wiring::persistent(model, Box::new(sink), sessions_dir()))?;

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

    // 构建 system prompt 与工具 workspace
    let cwd = config.cwd.clone();
    let system_prompt = prompt::build_system_prompt(&cwd);
    let workspace = config.cwd.clone();

    // 手工建 runtime（而非 `#[tokio::main]`）：TUI 模式的 UI 循环**必须在
    // 没有 runtime 上下文的线程上跑**（它用 `blocking_send`）。
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    match args.mode {
        Mode::Print(task) => {
            let (sink, rx) =
                ChannelSink::new(channel_capacity(), LifecyclePolicy::StopWhenConsumerGone);
            // 装箱前取读数句柄：装箱后仍能读到终态背压计数。
            let stats_handle = sink.stats_handle();
            let mut agent = build_agent(system_prompt, workspace)?;
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
            let (sink, rx) =
                ChannelSink::new(channel_capacity(), LifecyclePolicy::StopWhenConsumerGone);
            let stats_handle = sink.stats_handle();
            let mut agent = build_agent(system_prompt, workspace)?;
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
            run_interactive(rt, config, state_store, system_prompt, workspace)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use ys_core::{ContentBlock, Message, Role, StopReason, ToolCall, ToolCallId, Usage};
    use ys_model::MockModel;
    use ys_protocol::Source;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
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
        let mut agent = build_agent(String::new(), PathBuf::from(".")).unwrap();
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
        let mut agent = build_agent(String::new(), PathBuf::from(".")).unwrap();
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
}
