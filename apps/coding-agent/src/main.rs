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
//! 初始化顺序（含 R7 的阻塞提前返回）。`main` 的实际次序：
//!
//! ```text
//! 0. parse_args（含 --resume 形态）
//! 1. Config::from_env → registry.load_auth → 用户配置（config.json）
//!    → skill 扫描（default_skill_roots + load_skills + catalogue）
//! 2. state.json → sessions_dir → resolve_resume（`--resume <id>` 解析 / 裸 `--resume` 走 picker）
//! 3. 模型解析（env > meta > state.json > 首个有凭证）→ resolve_limits → set_model_factory
//! 4. WebConfig::from_env → 启动期清理（仅 Primary 档）
//!    → 装工具表（toolset 探测一次）→ 由工具表派生 system prompt → 合并启动期提示
//! 5. 建 runtime（三模式共用；UI 循环不在它上面跑，见上文线程模型）
//! 6. 按模式分发：
//!    - `-p` / `--json`：建 ChannelSink + Agent + Wiring → 跑单回合
//!    - TUI：建 ChannelSink + Agent + 【阻塞】Wiring → view₀
//!           → 建三条信道 request / boundary / outbound → spawn app 线程 + 当前线程跑 UI
//! ```
//!
//! `--resume` 解析与模型解析必须在**建 runtime 之前**完成：meta 参与模型选择，
//! 且 picker 的一切输出走 stderr（起屏前才安全，设计 §5.4/D12）。第 4 步的工具表
//! 既进 agent，也派生提示词清单 —— 同一份表，顺序自然一致（设计 §1.2 / §1.5）。
//!
//! 注意 **Agent 先于 Wiring 建**（`run_interactive` 的实际顺序）：二者互不依赖，
//! 而 `view₀` 要同时借用它们，故都放在 view₀ 之前。承重的不变量是「`begin_turn`
//! 只可能在 `rt.spawn` 之后发生」（`view₀` 因此必然看到 turn 恒 0），与这两者
//! 的先后无关。

mod app_loop;
mod assembly;
mod capabilities;
mod channel;
mod config;
mod logging;
mod prompt;
mod provider;
mod round_clock;
mod sessions;
mod skills;
mod state;
mod status;
mod subagent;
mod toolset;
mod view;
mod wiring;
mod worktree;

/// 测试专用：进程级 env 互斥与恢复，供各模块测试复用。
#[cfg(test)]
mod test_env;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use ys_core::StopReason;
use ys_event::AgentEvent;
use ys_loop::{AgentInput, LoopError, RunResult};
use ys_model_openai_compat::{
    OpenAICompatibleConfig, OpenAICompatibleModel, RetryConfig, TimeoutConfig,
};
use ys_protocol::{Boundary, Envelope, LifecyclePolicy, Outbound, QueueBoundarySource, Request};
use ys_runtime::{Agent, AgentBuilder, AgentPorts, BuildError};
use ys_session::SessionError;
use ys_tool::Tool;
use ys_tools_web::WebConfig;
use ys_tui_coding::CodingView;

use channel::{ChannelSink, ChannelStatsHandle};
use round_clock::RoundClock;
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

/// 启动期会话形态。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Resume {
    /// 新会话（默认）。
    New,
    /// `--resume`：交互式列表选择。
    Pick,
    /// `--resume <id>`：指定会话（id 或路径，空格形态）。
    Id(String),
}

/// `worktree` 子命令（CLI 四动词）。
///
/// `create` / `attach` 的名参可省（`None` ⇒ 自动命名 / 列表选取）；`list` 无参；
/// `remove` 必须有名字。
#[derive(Debug, Clone, PartialEq, Eq)]
enum WorktreeCmd {
    /// `worktree create [name]`：建树（`None` ⇒ `worktree::auto_name()`）。
    Create(Option<String>),
    /// `worktree attach [name]`：启动即绑定工作区（`None` ⇒ 列表选取）。
    Attach(Option<String>),
    /// `worktree list`：列出受管工作树。
    List,
    /// `worktree remove <name>`：删树（留分支、脏则拒删）。
    Remove(String),
}

/// 解析后的命令行参数。
#[derive(Debug, PartialEq, Eq)]
struct Args {
    mode: Mode,
    /// `--stats`：显式要求把背压读数摘要写到 stderr（无论有无背压）。
    stats: bool,
    /// 启动期会话形态。
    resume: Resume,
    /// `worktree` 子命令；`None` = 未走该子命令。
    worktree: Option<WorktreeCmd>,
}

/// 循环剥离「模式 flag 之后、任务文本之前」的 flag 区。
///
/// 只认**紧邻**位置（`-p --stats --resume 2026-10-07-abc "任务"`）：任务文本一旦开始，后续
/// `--stats` / `--resume` 就是文本的一部分（`-p "任务" --stats` → 文本
/// `"任务 --stats"`），从而保住「任务文本逐字保留」的既有语义。
///
/// `--resume` 取**紧随其后**的 token 为 id（空格形态 `--resume <id>`）—— 但仅当其后**还留有
/// 任务文本**（至少还有一个 token）；否则它是裸 `--resume`（列表选择），紧跟的 token 归任务。
/// 理由：`-p` / `--json` 的任务文本**必需**，`--resume` 绝不能把它吃掉 ⇒ `--resume <id> <任务>`
/// 取 id，`--resume <任务>`（其后无 token）是裸 flag + 任务文本。下一 token 不存在或以 `-`
/// 开头同样 ⇒ 裸 `--resume`。`--resume=<id>`（`=` 形态）**已移除** ⇒ 报错。
///
/// 对 `--stats` 与 `--resume` **顺序无关**（循环剥离）。重复 flag **last-wins 且静默
/// 丢弃**：这是有意保留的语义，不做冲突报错。
fn split_leading_flags(rest: &[String]) -> Result<(bool, Resume, &[String]), String> {
    let mut stats = false;
    let mut resume = Resume::New;
    let mut idx = 0;
    while idx < rest.len() {
        match rest[idx].as_str() {
            "--stats" => {
                stats = true;
                idx += 1;
            }
            "--resume" => {
                // 空格形态 `--resume <id>`；id **可选**（缺省 ⇒ 裸 `--resume` = 列表选择）。
                // `-p` / `--json` 的任务文本是**必需的** ⇒ 只有当 `--resume` 之后**还留着
                // 任务文本**（至少还有一个 token）时，紧跟的那个 token 才当 id，绝不把任务
                // 吃掉。于是 `--resume <id> <任务>` 取 id，`--resume <任务>`（其后无 token）
                // 是裸 flag + 任务文本。
                match rest.get(idx + 1).filter(|t| !t.starts_with('-')) {
                    Some(id) if idx + 2 < rest.len() => {
                        resume = Resume::Id(id.clone());
                        idx += 2;
                    }
                    _ => {
                        resume = Resume::Pick;
                        idx += 1;
                    }
                }
            }
            s if s.starts_with("--resume=") => {
                return Err("--resume 改为空格形态：`--resume <id>`（不再支持 `=`）。\
                     例如：-p --resume 2026-10-07-abc \"任务\""
                    .into());
            }
            _ => break,
        }
    }
    Ok((stats, resume, &rest[idx..]))
}

/// 解析 argv（不含 argv[0]）。
///
/// **只在首个参数位置判定模式 flag**：任务文本本身可能以 `-` 开头，
/// 例如 `-p --help me` 的任务文本就是 `--help me`。
/// 例外是模式 flag 之后的 flag 区——见 [`split_leading_flags`]。
///
/// TUI 形态：首个位置可为裸 `--resume`（列表选择）或 `--resume <id>`（指定会话，**空格形态**），
/// 其后参数照旧忽略。`--resume=<id>`（`=` 形态）已移除 ⇒ 报错。
fn parse_args(argv: &[String]) -> Result<Args, String> {
    let Some(first) = argv.first().map(String::as_str) else {
        return Ok(new_interactive());
    };

    match first {
        "-p" => {
            let (stats, resume, rest) = split_leading_flags(&argv[1..])?;
            let task = rest.join(" ");
            if task.is_empty() {
                Err("-p 需要一个任务参数，例如：ys-coding-agent -p \"修复这个 bug\"".into())
            } else {
                Ok(Args {
                    mode: Mode::Print(task),
                    stats,
                    resume,
                    worktree: None,
                })
            }
        }
        "--json" => {
            let (stats, resume, rest) = split_leading_flags(&argv[1..])?;
            let task = rest.join(" ");
            if task.is_empty() {
                Err("--json 需要一个任务参数，例如：ys-coding-agent --json \"修复这个 bug\"".into())
            } else {
                Ok(Args {
                    mode: Mode::Json(task),
                    stats,
                    resume,
                    worktree: None,
                })
            }
        }
        // `--resume <id>`（空格形态）或裸 `--resume`（列表选择）。
        "--resume" => Ok(Args {
            mode: Mode::Interactive,
            stats: false,
            resume: match argv.get(1).filter(|t| !t.starts_with('-')) {
                Some(id) => Resume::Id(id.clone()),
                None => Resume::Pick,
            },
            worktree: None,
        }),
        "worktree" => parse_worktree(&argv[1..]),
        // `=` 形态已移除：给出可操作的错误，而不是当成不认识的 flag。
        f if f.starts_with("--resume=") => {
            Err("--resume 改为空格形态：`--resume <id>`（不再支持 `=`）".into())
        }
        // 未知 flag：仅当它出现在**首个**位置时才算 flag（其余位置属任务文本）。
        // 单独的 `-` 视为普通参数（约定：读 stdin 的占位，本轮不支持）。
        f if f.starts_with('-') && f != "-" => Err(format!(
            "未知参数：{f}（可用：-p [--stats] [--resume <id>] <任务> | --json ... | [--resume [<id>]]）"
        )),
        // 无 flag：交互式。**非 flag 首 token 的静默 Interactive 行为保持不变**
        // （`worktree` 已由上面的臂截获，故此兜底只处理任务无关的裸词）。
        _ => Ok(new_interactive()),
    }
}

/// 默认的交互式 [`Args`]（无 worktree 子命令）。
fn new_interactive() -> Args {
    Args {
        mode: Mode::Interactive,
        stats: false,
        resume: Resume::New,
        worktree: None,
    }
}

/// 解析 `worktree` 子命令（`argv` 为 `worktree` 之后的 token）。
///
/// 未知动词 ⇒ `Err`。`create` / `attach` 的名参可选（`create` 多参 / `remove` 缺名 ⇒ `Err`）。
fn parse_worktree(argv: &[String]) -> Result<Args, String> {
    let Some(verb) = argv.first().map(String::as_str) else {
        return Err(
            "worktree 需要一个动词：create [name] | attach [name] | list | remove <name>".into(),
        );
    };
    match verb {
        "create" => Ok(Args {
            worktree: Some(WorktreeCmd::Create(optional_name(&argv[1..], "create")?)),
            ..new_interactive()
        }),
        "list" => {
            if argv.len() > 1 {
                return Err("worktree list 不接受参数".into());
            }
            Ok(Args {
                worktree: Some(WorktreeCmd::List),
                ..new_interactive()
            })
        }
        "remove" => {
            let Some(name) = argv.get(1).filter(|_| argv.len() == 2) else {
                return Err("worktree remove 需要一个名字：worktree remove <name>".into());
            };
            Ok(Args {
                worktree: Some(WorktreeCmd::Remove(name.clone())),
                ..new_interactive()
            })
        }
        "attach" => parse_worktree_attach(&argv[1..]),
        other => Err(format!(
            "未知的 worktree 动词：{other}（可用：create / attach / list / remove）"
        )),
    }
}

/// `create [name]` 的可选名参：0 参 ⇒ `None`（自动命名），1 参 ⇒ `Some`，更多 ⇒ `Err`。
///
/// **以 `-` 开头的名参一律报错**：名参与模式 flag 在语法上无从区分，若不拦，单独一个
/// token（如 `worktree create -p`）会被当成名字、建出一棵名为 `-p` 的树。要自动命名就省略名字。
fn optional_name(rest: &[String], verb: &str) -> Result<Option<String>, String> {
    match rest {
        [] => Ok(None),
        [name] if name.starts_with('-') => Err(format!(
            "worktree {verb} 的名字不能以 `-` 开头；如需自动命名请省略名字"
        )),
        [name] => Ok(Some(name.clone())),
        _ => Err(format!("worktree {verb} 至多接受一个名字参数")),
    }
}

/// `attach [name] [模式 flag...]`：名参可选（首 token 是 flag 则无名），其后按顶层模式解析
/// ——`attach` 可带 `-p` / `--json` / `--resume [<id>]`（设计 §5.1）。
fn parse_worktree_attach(rest: &[String]) -> Result<Args, String> {
    let (name, tail) = match rest.first().map(String::as_str) {
        None => (None, &rest[0..]),
        Some(s) if s.starts_with('-') && s != "-" => (None, rest),
        Some(_) => (Some(rest[0].clone()), &rest[1..]),
    };
    let (mode, stats, resume) = parse_mode_tail(tail)?;
    Ok(Args {
        mode,
        stats,
        resume,
        worktree: Some(WorktreeCmd::Attach(name)),
    })
}

/// 解析 `worktree attach` 名参之后的模式 flag 区（照顶层 `parse_args` 的形态）。
fn parse_mode_tail(tail: &[String]) -> Result<(Mode, bool, Resume), String> {
    let Some(first) = tail.first().map(String::as_str) else {
        return Ok((Mode::Interactive, false, Resume::New));
    };
    match first {
        "-p" => {
            let (stats, resume, rest) = split_leading_flags(&tail[1..])?;
            let task = rest.join(" ");
            if task.is_empty() {
                Err("-p 需要一个任务参数".into())
            } else {
                Ok((Mode::Print(task), stats, resume))
            }
        }
        "--json" => {
            let (stats, resume, rest) = split_leading_flags(&tail[1..])?;
            let task = rest.join(" ");
            if task.is_empty() {
                Err("--json 需要一个任务参数".into())
            } else {
                Ok((Mode::Json(task), stats, resume))
            }
        }
        "--resume" => Ok((
            Mode::Interactive,
            false,
            match tail.get(1).filter(|t| !t.starts_with('-')) {
                Some(id) => Resume::Id(id.clone()),
                None => Resume::Pick,
            },
        )),
        f if f.starts_with("--resume=") => {
            Err("--resume 改为空格形态：`--resume <id>`（不再支持 `=`）".into())
        }
        other => Err(format!(
            "worktree attach 只接受 -p / --json / --resume [<id>] 与可选名字；未知参数：{other}"
        )),
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
    // 逐轮读数（`crate::round_clock`）：`-p` / `--json` 与 TUI 落同一套行，跨入口可比。
    // 时钟建在**消费者**里，是因为两条入口的消费点不同（TUI 是 `app_loop` 的事件泵）。
    let mut clock = RoundClock::default();
    while let Some(env) = rx.recv().await {
        if let Some(line) = clock.observe(&env, Instant::now()) {
            logging::log(&line);
        }
        on_event(&mut w, &env)?;
        w.flush()?;
        if is_terminal(&env.event) {
            // 兜底：终局事件没能关闭某轮（理论上不会）时仍落一行。
            if let Some(line) = clock.take_line() {
                logging::log(&line);
            }
            return Ok(());
        }
    }
    if let Some(line) = clock.take_line() {
        logging::log(&line);
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
pub(crate) fn build_agent(
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
    limits.max_rounds = config.max_rounds();
    // 回合级工具结果预算：取窗口的 1/4（128k → 32k）。不新增 config 字段 —— 初值即可，
    // 真实会话再校准；`0` 会关闭闸门，故必须显式设非零。
    limits.max_turn_tool_result_tokens = config.context_window() / 4;
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
pub(crate) fn fixed_prompt_tokens(system_prompt: &str, specs: &[ys_tool::ToolSpec]) -> usize {
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

/// 默认新会话、但会话目录非空时，把一行 `Resume with --resume` 可发现性提示追加进
/// 既有启动期提示（设计 §9）。
///
/// **绝不覆盖**已有提示：`triggered` 为假时原样返回 `hint`；为真时多行拼接、
/// 原有提示在前（与 [`startup_hints`] 的「一行一事」同规）。
fn with_resume_hint(hint: Option<String>, triggered: bool) -> Option<String> {
    if !triggered {
        return hint;
    }
    let line = "Resume with --resume".to_string();
    Some(match hint {
        Some(h) => format!("{h}\n{line}"),
        None => line,
    })
}

/// `-p` / `--json` 的 task 展开：与 TUI 的 `Request::Prompt` 走**同一套**逻辑
/// （`skills::expand_invocation`），三模式行为一致（设计 §关键数据流）。
fn expand_task(task: String, skill_entries: &[skills::SkillEntry]) -> String {
    skills::expand_invocation(&task, skill_entries).unwrap_or(task)
}

/// `-p` / `--json`：标题素材取 **skill 展开之前**的原文（C3），展开只用于送给
/// 模型的正文。
///
/// 两臂共用 —— 「先留底稿、再展开」的顺序是 **C3 的唯一保证点**：`raw` 进
/// [`finalize_turn_meta`] 落盘为 `title`，`expanded` 进 `run_turn` 送给模型；
/// 顺序反了就会把 `<skill …>` 信封（可达 9k 正文）当成标题。
fn raw_and_expanded_task(task: String, skills: &[skills::SkillEntry]) -> (String, String) {
    let raw = task.clone();
    (raw, expand_task(task, skills))
}

/// 由 meta 播种回合统计（resume 后状态行的累计不归零；`Envelope.turn` 仍从 1 起）。
pub(crate) fn seed_stats(wiring: &Wiring) -> status::TurnStats {
    let mut stats = status::TurnStats::default();
    if let Some(m) = wiring.meta() {
        stats.total_input_tokens = m.input_tokens;
        stats.total_output_tokens = m.output_tokens;
        stats.turn_count = m.turns;
    }
    stats
}

/// 回合末写穿会话 meta（**三模式共用**，设计 C7）。
///
/// - `title`：仅当缺省且素材非空时写 `derive_stored_title(raw)`（幂等，C2）；
///   素材须是 **skill 展开之前**的用户原文（C3）。
/// - `provider` / `model`：**仅 `Some` 时写**。env 凭证路径下 `config.provider`
///   恒为 `None`，无条件写会把 `/login` 落到 meta 的 provider 清空 —— 设计 D10
///   第 2 级恢复（`meta.provider` + `registry.auth_for`）便永久失效。
/// - `thinking`：仅 `Some` 时写（`-p`/`--json` 不参与 thinking 开关）。
/// - `workspace`：**仅某种 attach 状态时写**（`Some` = 本次工作区绑定到该树路径）。
///   `None`（共享工作区）**不写** —— 把共享工作区（= 启动 cwd）记进 meta 是无用噪声，
///   且下次 `--resume` 会据此做一次无意义的「恢复」。
/// - tokens / turns：无条件写 `stats`。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn finalize_turn_meta(
    wiring: &mut Wiring,
    raw_user_text: Option<&str>,
    provider: Option<&str>,
    model: Option<&str>,
    thinking_visible: Option<bool>,
    workspace: Option<&Path>,
    stats: &status::TurnStats,
) -> Result<(), SessionError> {
    let title = raw_user_text
        .map(sessions::derive_stored_title)
        .filter(|t| !t.is_empty());
    wiring
        .update_meta(|meta| {
            if meta.title.is_none() {
                if let Some(t) = title {
                    meta.title = Some(t);
                }
            }
            if let Some(p) = provider {
                meta.provider = Some(p.to_owned());
            }
            if let Some(m) = model {
                meta.model = Some(m.to_owned());
            }
            if let Some(tv) = thinking_visible {
                meta.thinking_visible = tv;
            }
            if let Some(w) = workspace {
                meta.workspace = Some(w.to_path_buf());
            }
            meta.input_tokens = stats.total_input_tokens;
            meta.output_tokens = stats.total_output_tokens;
            meta.turns = stats.turn_count;
        })
        .await
}

/// 会话文件 stem（`session: <id>` 提示用）。
fn session_id_of(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
}

/// 退出时的会话提示行：`session: <id>`；`None` = 不打。
///
/// **只在会话文件已落盘时才打** —— 默认新会话是惰性的（首次追加才落盘），一个回合
/// 都没跑过的会话压根没有文件，打出的 id 连 `--resume` 都找不到（`sessions::resolve`
/// 会报「找不到会话」），那是误导而不是提示。
///
/// 三模式共用同一条：`-p` / `--json` 收尾与 TUI 退出后（`run_interactive`）都经此，
/// 措辞因此不可能漂移。三处都写 **stderr**（stdout 属数据流：`-p` 是模型文本、
/// `--json` 是逐行 `Envelope`、TUI 是退出回放的 transcript）。
fn session_exit_line(path: Option<&Path>) -> Option<String> {
    let path = path.filter(|p| p.is_file())?;
    Some(format!("session: {}", session_id_of(path)?))
}

/// 退出时的工作树提示行：`worktree: <name>`（未绑定任何树时不打）。
///
/// 与 [`session_exit_line`] 并列、同落 **stderr**。名字取自
/// [`app_loop::WorktreeHandle`] —— 工作区在**运行期可变**（会话内 `/resume` 可切换），
/// 故退出行不能由启动期局部量给出，必须与回合末的 meta 写入同源。
fn worktree_exit_line(name: &str) -> String {
    format!("worktree: {name}")
}

/// 两路径是否指向**同一目录**：能 canonicalize 就按规范化结果比，否则按字面比。
///
/// 用于「显式 attach 的树」与「meta 记录的树」是否同一个 —— 二者来源不同
/// （一者经 `path_for` 拼接、一者来自落盘 meta），字面可能差在符号链接 / `..` 上。
pub(crate) fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    norm(a) == norm(b)
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

/// 起屏前解析 `--resume`：返回恢复路径（`None` = 默认新会话）与其 meta。
///
/// 空列表按**默认新会话**处理（stderr 提示，不报错）；`resolve` / `pick` 的 Err
/// ⇒ stderr 报错后退出。
fn resolve_resume(
    resume: &Resume,
    dir: &Path,
) -> (Option<PathBuf>, Option<ys_session::SessionMeta>) {
    let path = match resume {
        Resume::New => None,
        Resume::Id(id) => {
            if id.is_empty() {
                eprintln!("--resume 需要一个会话 id（例如 --resume 2026-10-07-abc）");
                std::process::exit(1);
            }
            match sessions::resolve(dir, id) {
                Ok(p) => Some(p),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        Resume::Pick => {
            let entries = sessions::list(dir);
            if entries.is_empty() {
                eprintln!("没有可恢复的会话");
                None
            } else {
                match sessions::pick(&entries) {
                    Ok(p) => Some(p),
                    Err(e) => {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
                }
            }
        }
    };
    let meta = path.as_deref().and_then(sessions::read_meta);
    (path, meta)
}

/// 会话目录是否已有条目（仅用于启动期 `Resume with --resume` 提示）：只读一个
/// 条目，不做列表 / 解析。
fn sessions_dir_has_entries(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

/// 模型选择的**纯解析结果** —— [`resolve_model_from_meta`] 的产物。
///
/// 纯数据：调用方决定何时写回 `Config`（启动期立即写；运行期 `/resume` 在提交步写）。
pub(crate) struct ModelChoice {
    pub provider: Option<String>,
    pub api_base: String,
    pub api_key: String,
    pub model: String,
}

/// 按**既定优先级链**纯解析出要用的模型四字段（**不写 `cfg`**）：
///
/// 1. **env 完整凭证**（`cfg.is_configured()`）⇒ 原样取 `cfg` 的四字段；
/// 2. `meta` 的 `provider` + `model`（`provider` 须 `registry.auth_for` 命中）；
/// 3. `state` 的 last-active `provider` + `model`（`provider` 须为**已知 provider**且
///    有凭证）；
/// 4. 第一个存有凭证的内置 provider。
///
/// 某档的 provider 无凭证时**降级到下一档而非失败**；全部耗尽 ⇒ `None`。启动期与
/// 运行期 `/resume` 共用这一条链（设计 §5.6 / §3.3）。
pub(crate) fn resolve_model_from_meta(
    cfg: &config::Config,
    meta: Option<&ys_session::SessionMeta>,
    state: Option<&state::AppState>,
) -> Option<ModelChoice> {
    // 档 1：env 完整凭证 —— 原样保留（等价于启动期的 `if !is_configured()` 短路）。
    if cfg.is_configured() {
        return Some(ModelChoice {
            provider: cfg.provider.clone(),
            api_base: cfg.api_base.clone().unwrap_or_default(),
            api_key: cfg.api_key.clone().unwrap_or_default(),
            model: cfg.model.clone(),
        });
    }

    // 档 2：resume meta（provider 须有 auth entry；不看是否为已知 provider）。
    if let Some(provider) = meta.and_then(|m| m.provider.as_deref())
        && let Some(entry) = cfg.registry.auth_for(provider)
    {
        let model = meta
            .and_then(|m| m.model.clone())
            .unwrap_or_else(|| entry.model.clone());
        return Some(ModelChoice {
            provider: Some(provider.to_string()),
            api_base: entry.api_base.clone(),
            api_key: entry.api_key.clone(),
            model,
        });
    }

    // 档 3：state.json 的 last-active（provider 须为已知 provider 且有凭证）。
    if let Some(state) = state
        && let Some(recovered) = state
            .last_active_provider
            .as_deref()
            .and_then(|name| cfg.registry.find_provider(name))
            .zip(
                state
                    .last_active_provider
                    .as_deref()
                    .and_then(|name| cfg.registry.auth_for(name)),
            )
    {
        let (provider, entry) = recovered;
        let model = state
            .last_active_model
            .clone()
            .unwrap_or_else(|| entry.model.clone());
        return Some(ModelChoice {
            provider: Some(provider.name.clone()),
            api_base: entry.api_base.clone(),
            api_key: entry.api_key.clone(),
            model,
        });
    }

    // 档 4：第一个存有凭证的内置 provider。
    for provider in cfg.registry.providers() {
        if let Some(entry) = cfg.registry.auth_for(&provider.name) {
            return Some(ModelChoice {
                provider: Some(provider.name.clone()),
                api_base: entry.api_base.clone(),
                api_key: entry.api_key.clone(),
                model: entry.model.clone(),
            });
        }
    }

    None
}

/// 起屏前拉不到模型目录时的统一说法：**点名要求**（不再有内置默认名）。
fn catalog_unavailable(base: &str, reason: &str) -> String {
    format!(
        "无法从 {base} 读取模型目录（{reason}）；模型名不再有内置默认值 —— \
         请显式设置 YUSHAN_MODEL，或在 TUI 里用 `/model <provider>/<name>` 指定"
    )
}

/// 把目录里的**第一条**当起始模型：写进 `config.model`，并把整份目录并入 registry
/// 的目录缓存 —— `/model` 候选（`models_for_provider`）与 `context_window_for` 的
/// ② 级（目录里厂商自己报的窗口）立刻可用。
///
/// 空目录 ⇒ `Err(原因)`：**不猜**（既不改 `config.model`，也不动端点）。
fn adopt_first_catalog_model(
    config: &mut config::Config,
    base: &str,
    models: Vec<provider::ApiModel>,
) -> Result<(), &'static str> {
    let Some(first) = models.first() else {
        return Err("端点返回了空目录");
    };
    config.model = first.id.clone();
    let mut cache = provider::ModelCache::new();
    cache.insert(base.trim_end_matches('/').to_string(), models);
    config.registry.apply_model_cache(cache);
    Ok(())
}

/// `-p` / `--json` 单回合收尾：由 meta 播种统计、`record(usage)` 后写穿 meta，
/// 并把 `session: <id>`（以及 `worktree: <name>`，若在树内）打到 **stderr**
/// （stdout 属数据流，不得污染）。
///
/// `binding` 由**工作区句柄**读出（`Some` = 绑定到某棵树），meta 的 `workspace` 与退出行
/// 同源于它 —— 二者不可能各说一套。
async fn persist_single_turn(
    wiring: &mut Wiring,
    raw_task: &str,
    provider: Option<&str>,
    binding: Option<&app_loop::WorktreeBinding>,
    result: &RunResult,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut stats = seed_stats(wiring);
    stats.record(&result.usage);
    let model = wiring.model_id().map(str::to_string);
    finalize_turn_meta(
        wiring,
        Some(raw_task),
        provider,
        model.as_deref(),
        None,
        binding.map(|b| b.path.as_path()),
        &stats,
    )
    .await?;
    if let Some(line) = session_exit_line(wiring.session_path()) {
        eprintln!("{line}");
    }
    if let Some(b) = binding {
        eprintln!("{}", worktree_exit_line(&b.name));
    }
    Ok(())
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
    eprintln!("  YUSHAN_MODEL     — Model name (unset: 取该端点 /v1/models 的第一条)");
    eprintln!("Or run in interactive mode and use /login to configure.");
    std::process::exit(1);
}

/// `-p` / `--json` 的驱动：跑**一个** turn。
///
/// `Inbox` / `Agent::run` 已删（T1）：单次运行就是一次 `run_turn`，`begin_turn(1)`
/// 让 `Envelope.turn` 从 1 起（不再有逐回合递增的多回合循环）。
///
/// **wall-clock 预算**：`deadline = Instant::now() + budget`，本路径只有一个回合，
/// 故只算一次。到点**先**向 **stderr** 写说明行（stdout 承载模型增量流 / JSON 流，
/// 必须保持干净），**再**向本回合新建的边界源 `push(Boundary::Abort)`；回合在轮
/// 边界看到后以 `Cancelled` 收场。`budget` 由调用方取自
/// `config.max_turn_duration()`，**绝不写死**。
async fn run_single_turn(
    agent: &mut Agent,
    wiring: &mut Wiring,
    input: AgentInput,
    budget: Duration,
    config: Option<&config::Config>,
    subagent: Option<&subagent::SubagentSetup>,
) -> Result<RunResult, LoopError> {
    // 预算计时原点与本回合耗时原点**同一个** —— 两个 `Instant::now()` 会让
    // `elapsed` 与 `budget_remaining` 对不上账。
    let started = Instant::now();
    let deadline = started + budget;
    // 本回合专属的边界源：`Abort` 的标志位随回合作废，绝不跨回合复用。
    let boundary = QueueBoundarySource::new();
    let ports = wiring.ports_with_boundary(Some(&boundary));
    ports.events.begin_turn(1);

    // subagent 能力：每回合造一份 runner（派发计数器天然 per-turn）。
    let totals = std::sync::Arc::new(subagent::UsageTotals::default());
    let runner = subagent.map(|s| s.make_runner(config, ports.model, totals.clone()));
    let mut agent_ports =
        AgentPorts::new(ports.model, ports.session, ports.events, Some(&boundary));
    if let Some(r) = &runner {
        agent_ports = agent_ports.with_subagent(r);
    }

    let turn = agent.run_turn(input, agent_ports);
    tokio::pin!(turn);
    // 预算计时器**只允许触发一次**：到点后置位，`select!` 的 `if` 前置条件随即
    // 禁用该分支（`sleep_until` 对已过去的 deadline 会持续 Ready ⇒ 否则忙等 +
    // 重复发说明行 + 重复 push `Abort`）。
    let mut budget_exhausted = false;
    let outcome = loop {
        tokio::select! {
            result = &mut turn => break result,
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)),
                if !budget_exhausted => {
                budget_exhausted = true;
                let secs = budget.as_secs();
                eprintln!(
                    "budget exhausted: wall-clock budget of {secs}s elapsed — aborting this turn"
                );
                boundary.push(Boundary::Abort);
            }
        }
    };
    // 子 agent 的 usage **上卷**：预算独立、记账上卷（ADR-0029 决策）。
    let outcome = match outcome {
        Ok(mut r) => {
            r.usage = r.usage + totals.drain();
            // 空产出在**一次性模式**下必须留一行可见说明（`-p` 只打文本增量，否则 stderr
            // 与 stdout 都为空 = 静默）。退出码由 [`single_turn_exit_code`] 给 `1`。
            if r.stop_reason == StopReason::NoOutput {
                eprintln!(
                    "model returned no visible output this turn (nothing to print) — retry or rephrase"
                );
            }
            Ok(r)
        }
        Err(e) => Err(e),
    };
    // 回合边界读数（与 TUI 入口同格式，跨入口可 grep 比对）。
    // **只在成功路径落**：失败回合拿不到 `RunResult`，写 `rounds=0` 会把「没跑到」与
    // 「跑了 N 轮才失败」混同 —— 那一回合的轮数由逐轮行给出（见 `crate::round_clock`）。
    if let Ok(r) = &outcome {
        logging::log(&round_clock::turn_line(
            1,
            r.rounds,
            started.elapsed(),
            r.rounds,
            deadline.saturating_duration_since(Instant::now()),
            round_clock::turn_cache(&outcome),
        ));
    }
    outcome
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
    budget: Duration,
    stats: &ChannelStatsHandle,
    force_stats: bool,
    config: &config::Config,
    subagent: Option<&subagent::SubagentSetup>,
) -> Result<RunResult, Box<dyn std::error::Error>> {
    ensure_configured(wiring)?;

    // 消费与跑回合必须**并发**：有界信道若无并发消费者，agent 撞满即等待 → 死锁。
    let (run_result, consume_result) = tokio::join!(
        run_single_turn(
            agent,
            wiring,
            AgentInput::text(task.as_str()),
            budget,
            Some(config),
            subagent,
        ),
        consume_print_events(rx, io::stdout()),
    );

    // 消费已排空（join 返回）→ 此刻读读数为终态。先报告，再传播错误。
    report_channel_stats(stats, force_stats);
    let result = run_result?;
    let printed_any = consume_result?;

    if printed_any {
        println!();
    }
    Ok(result)
}

/// `--json` 模式：并发消费事件流，逐行输出 `Envelope` 的 JSON。
///
/// 背压读数走 stderr（[`report_channel_stats`]），不混入 stdout 的 JSON 流。
async fn run_json_mode(
    agent: &mut Agent,
    wiring: &mut Wiring,
    rx: mpsc::Receiver<Envelope>,
    task: String,
    budget: Duration,
    stats: &ChannelStatsHandle,
    force_stats: bool,
    config: &config::Config,
    subagent: Option<&subagent::SubagentSetup>,
) -> Result<RunResult, Box<dyn std::error::Error>> {
    ensure_configured(wiring)?;

    let (run_result, consume_result) = tokio::join!(
        run_single_turn(
            agent,
            wiring,
            AgentInput::text(task.as_str()),
            budget,
            Some(config),
            subagent,
        ),
        consume_events(rx, io::stdout(), write_json_envelope),
    );

    report_channel_stats(stats, force_stats);
    let result = run_result?;
    consume_result?;
    Ok(result)
}

/// 一次性模式的退出码：**没有可用产出**或**预算用尽** ⇒ `1`，否则 `0`。
///
/// - `Cancelled`（wall-clock 预算到点）、`NoOutput`（模型没给出可见内容）⇒ `1`；
/// - `Completed` / `MaxRounds` / `Truncated`（有可交差的输出，或正常收尾）⇒ `0`。
///
/// 判据是「**有没有输出能交差**」：`Truncated` 至少有一段（可能不完整的）输出，故为 `0`；
/// `NoOutput` 什么都没有 —— 若报 `0` 就是**静默装作成功**。
/// `Cancelled` 是这条路径上「预算用尽」的唯一信号（一次性模式没有别的 `Abort` 来源）。
fn single_turn_exit_code(result: &RunResult) -> i32 {
    match result.stop_reason {
        StopReason::Cancelled | StopReason::NoOutput => 1,
        _ => 0,
    }
}

/// 预算兜底退出的落地：非零退出码时**先 `flush` 再 `exit`**。
///
/// `-p` 的文本增量是直写 stdout 的（不带换行）。`process::exit` 会经 `rt::cleanup`
/// 冲刷 stdout（std 的 `process::exit` → `rt::cleanup` → `io::stdio`），故此处显式
/// `flush` **不是**补救缺陷，而是把「退出前 stdout 已排空」这一语义写显、并对未来
/// 的缓冲实现保持保守。说明行已由 [`run_single_turn`] 写到 stderr，这里**不打印
/// `Error:` 噪声**。
fn exit_with_code(code: i32) {
    if code != 0 {
        let _ = io::stdout().flush();
        std::process::exit(code);
    }
}

/// 启动期要入队的出站序列，按**入队顺序**排列。
///
/// **顺序不变量（I10）**：`--resume` 的回放（`SessionResumed`）必须排在启动降级提示
/// **之前** —— 反了的话，回放触发的 UI 清屏会把先入队的提示吃掉。这是启动期唯一
/// 给出该次序的地方，`run_interactive` 只按返回的次序投递。
fn startup_outbound_sequence(
    replay: Option<Outbound<CodingView>>,
    hint: Option<String>,
) -> Vec<Outbound<CodingView>> {
    let mut out = Vec::new();
    if let Some(msg) = replay {
        out.push(msg);
    }
    if let Some(hint) = hint {
        out.push(Outbound::Output(hint));
    }
    out
}

/// 启动期回放载荷：仅在**本次带 `--resume`**（`resuming`）时构造 `Outbound::SessionResumed`
/// —— 让恢复的会话历史上屏（设计 D3）。`history` / `thinking_visible` 取自接线器已恢复
/// 的会话。
fn resume_replay(wiring: &Wiring, resuming: bool) -> Option<Outbound<CodingView>> {
    if !resuming {
        return None;
    }
    let path = wiring.session_path()?;
    let meta = wiring.meta();
    Some(Outbound::SessionResumed {
        summary: capabilities::session_summary(path, meta),
        thinking_visible: meta.map(|m| m.thinking_visible).unwrap_or(false),
        history: wiring.session_messages().to_vec(),
    })
}

/// TUI 模式的全部组装（设计 §7 初始化顺序 1–5）。
///
/// **必须在非 async 线程上调用末尾的 UI 循环** —— 故这里只做「异步初始化 +
/// spawn app 线程 + 跑 UI」这三件事，`rt` 由调用方持有。
///
/// `agent` / `subagent` 是 [`assembly::AgentFactory`] 按启动工作区装配好的产物；
/// `factory` 与 `default_workspace` 交给 `app_loop`，供会话内 `/resume` 换工作区时
/// **重建** agent（[`app_loop::WorktreeHandle`] 承载运行期可变的工作区绑定，
/// `main` 退出后读它打 `worktree:` 行）。
#[allow(clippy::too_many_arguments)]
fn run_interactive(
    rt: tokio::runtime::Runtime,
    config: config::Config,
    state_store: state::StateStore,
    agent: Agent,
    subagent: subagent::SubagentSetup,
    factory: assembly::AgentFactory,
    startup_hint: Option<String>,
    sessions_dir: PathBuf,
    resume_path: Option<PathBuf>,
    worktree: app_loop::WorktreeHandle,
    default_workspace: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. 事件出口：有界信道 + 既定 policy（R7 的阻塞早退不影响它 —— 没 turn 就没有事件）。
    let (sink, sink_rx) =
        ChannelSink::new(channel_capacity(), LifecyclePolicy::StopWhenConsumerGone);

    // agent 已由装配工厂按启动工作区建好（工具表 + working_dir + system prompt 三者同源）；
    // session/model/events 经端口传入。

    // 2.【阻塞】会话 + model 构造 —— 失败会提前返回（此刻 turn 恒 0 也无所谓）。
    //    有 resume 路径 ⇒ 恢复它；否则默认新会话（惰性落盘）。
    let model = config.build_model();
    let wiring = rt.block_on(async {
        match resume_path.as_deref() {
            Some(path) => {
                Wiring::resume(model, Box::new(sink), sessions_dir, path.to_path_buf()).await
            }
            None => Wiring::persistent_new(model, Box::new(sink), sessions_dir).await,
        }
    })?;

    // 会话路径快照：UI 退出后打印 `session: <id>` 用。启动态在此播种（此刻只有
    // 这里知道 `persistent_new` 算出的文件名），`/new` 之后由 app 循环刷新。
    let session_path =
        app_loop::SessionPathHandle::new(wiring.session_path().map(Path::to_path_buf));

    // 2b.【后台】拉每个已登录 provider 的 `/v1/models`，缓存供 `/model` 浮层用。
    //
    // **不再阻塞起屏**：起屏前只摘出 `(api_base, api_key)` 目标，拉取在 TUI 起屏后
    // `rt.spawn` 到后台；目录到手经信道送回 app 循环（`app_loop::run` 的 `models_rx`）
    // 写进 registry 并刷新视图。渲染路径依旧一次 HTTP 都不发（设计 M4）—— 写与读同
    // 在 app 循环那一线程，后台任务只负责网络、不碰 registry（唯一写者不分裂）。
    //
    // `/model` 候选在目录回来前只有当前在跑的那条模型，回来后由视图刷新补上目录。
    // 5s 超时且并发；某一家拉不到就**没有**那一家（不再拿写死的名字充数）。
    // 无任何可拉目标时不拉（`models_rx = None`）。
    // env 直连（`provider` 无名）的端点也要拉：那条路径的 `/model` 候选与窗口查询
    // 同样只有「目录」一个来源，registry 里没有它的名字，只能按 base 认。
    let env_direct = if config.provider.is_none() {
        config.api_base.clone().zip(config.api_key.clone())
    } else {
        None
    };
    let fetch_targets = config.registry.fetch_targets(env_direct);
    let models_rx = if fetch_targets.is_empty() {
        None
    } else {
        let (models_tx, models_rx) = mpsc::channel::<provider::ModelCache>(1);
        rt.spawn(async move {
            let cache = provider::ProviderRegistry::fetch_all(fetch_targets).await;
            // app 循环已收摊（UI 退出）时无人接收 —— 静默丢弃即可。
            let _ = models_tx.send(cache).await;
        });
        Some(models_rx)
    };

    // 由 meta 播种累计统计（resume 后状态行的 tokens / turns 不归零）。
    // `session_started`（状态行时长）属展示、非执行状态，不恢复。
    let stats = seed_stats(&wiring);
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

    // 4b. 启动期入队：**先回放（`--resume`）、后降级提示**（顺序不变量 I10）。
    //
    // 带 `--resume` 时把历史经 `Outbound::SessionResumed` 交给 UI —— `App::new` 的
    // transcript 恒空，不回放则恢复的会话在屏上是空的（设计 D3）。
    //
    // **顺序不可反**：回放触发 UI 侧的清屏（`reset_for_replay`），若降级提示先入队，
    // 它会被这次清屏吃掉 —— 正是 I10 要防的 bug。故二者由
    // [`startup_outbound_sequence`] 统一给出次序，本地不再各投各的。
    //
    // 起屏前写 stderr 的行会留在主屏缓冲区里，退出 alt-screen 后又回到屏幕上，
    // 成为残渣（设计 §5 的「命令期间 → `Outbound::Output`」在启动期的延伸）。
    // 在 spawn 之前投递 ⇒ 它在信道里稳稳排在对话之前。
    // 容量 1024 且信道全新 ⇒ `try_send` 不可能失败；真失败也只落日志，不 panic。
    let replay = resume_replay(&wiring, resume_path.is_some());
    for msg in startup_outbound_sequence(replay, startup_hint) {
        if let Err(e) = out_tx.try_send(msg) {
            logging::log(&format!("failed to enqueue startup outbound: {e}"));
        }
    }

    // 5. app 线程（tokio worker）跑循环；当前线程跑**阻塞**的 UI 循环。
    //    app 循环唯一的失败是「UI 侧收摊」，不该惊动用户 —— 落日志即可。
    let app_session_path = session_path.clone();
    let app_worktree = worktree.clone();
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
            models_rx,
            Some(subagent),
            factory,
            default_workspace,
            app_session_path,
            app_worktree,
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
    // 宽限**先于**读快照：正在收尾的 `/new` / `/resume` 有机会把新值写进来。
    rt.shutdown_timeout(APP_THREAD_GRACE);

    // 退出提示：`session: <id>` 落到**主屏**（alt-screen 已由 TUI 退出时恢复），
    // 让用户拿得到刚才那轮的会话号去 `--resume`。与 `-p` / `--json` 同一行、同一去处
    // ——见 [`session_exit_line`]。
    if let Some(line) = session_exit_line(session_path.get().as_deref()) {
        eprintln!("{line}");
    }
    // `worktree:` 行取自**运行期可变**的工作区绑定快照（`/resume` 可能已切树）。
    if let Some(b) = worktree.get() {
        eprintln!("{}", worktree_exit_line(&b.name));
    }

    tui_result?;
    Ok(())
}

/// 解析 `attach` 的目标树路径与名字。
///
/// 指名 ⇒ 直接用；无名 ⇒ 从受管工作树中选取（**恰一棵**则用它，否则报错并**列出候选**）。
///
/// 0 棵 / 多棵都用 [`worktree::WorktreeError::NoAttachTarget`]（`Display` 即完整提示）：
/// 多棵时逐行给出**名字 + 路径**，用户可直接复制名字重试；0 棵时说明「本仓还没有工作树，
/// 先 `worktree create`」。调用方（`main`）对这条错误**降级**为共享工作区启动。
fn resolve_attach_target(
    cwd: &Path,
    name: Option<&str>,
) -> Result<(PathBuf, String), worktree::WorktreeError> {
    match name {
        Some(n) => worktree::attach_target(cwd, n).map(|p| (p, n.to_string())),
        None => {
            let repo = worktree::main_repo(cwd)?;
            let entries = worktree::list(&repo.root)?;
            match entries.as_slice() {
                [] => Err(worktree::WorktreeError::NoAttachTarget(
                    "本仓还没有工作树（NoAttachTarget）；先用 `ys-coding-agent worktree create` 建一棵，\
                     再 `worktree attach <名字>`（省略名字 = 从候选里选用唯一一棵）"
                        .into(),
                )),
                [only] => Ok((only.path.clone(), only.name.clone())),
                many => {
                    let mut msg = format!(
                        "本仓有 {} 个工作树（NoAttachTarget），请指名其一后重试：",
                        many.len()
                    );
                    for e in many {
                        msg.push_str(&format!("\n  {}  {}", e.name, e.path.display()));
                    }
                    Err(worktree::WorktreeError::NoAttachTarget(msg))
                }
            }
        }
    }
}

/// 执行 `worktree create` / `list` / `remove`：打印结果后**早退**（不建 runtime / 不建 agent）。
///
/// `create` / `remove` 是显式动作 ⇒ 失败即**硬失败**（stderr 写明原因 + 退路，退出码非零）。
/// `list` 结果写 **stdout**（数据流），仓库头写到 **stderr**（诊断）。
fn run_worktree_early(cmd: &WorktreeCmd, cwd: &Path) -> ! {
    match cmd {
        WorktreeCmd::Create(name) => match worktree::main_repo(cwd)
            .and_then(|repo| worktree::create(&repo.root, name.as_deref()))
        {
            Ok(entry) => {
                // `worktree: <name>` 落 stderr（与 `session: <id>` 同侧同风格）。
                eprintln!("{}", worktree_exit_line(&entry.name));
                eprintln!("路径：{}", entry.path.display());
                if let Some(branch) = &entry.branch {
                    eprintln!("分支：{branch}（未合并）");
                }
                eprintln!("下一步：ys-coding-agent worktree attach {}", entry.name);
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("创建 worktree 失败：{e}");
                eprintln!(
                    "退路：确认当前目录在 git 仓库内、已安装 git；若提示路径已被登记，可 `git worktree prune` 后重试。"
                );
                std::process::exit(1);
            }
        },
        WorktreeCmd::List => {
            let listed = worktree::main_repo(cwd).and_then(|repo| {
                eprintln!("仓库：{}（{}）", repo.name, repo.root.display());
                worktree::list(&repo.root)
            });
            match listed {
                Ok(entries) => {
                    for e in entries {
                        let branch = e.branch.as_deref().unwrap_or("-");
                        let state = if e.dirty { "dirty" } else { "clean" };
                        println!("{}\t{}\t{}\t{}", e.name, e.path.display(), branch, state);
                    }
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("列出 worktree 失败：{e}");
                    std::process::exit(1);
                }
            }
        }
        WorktreeCmd::Remove(name) => {
            match worktree::main_repo(cwd).and_then(|repo| worktree::remove(&repo.root, name)) {
                Ok(()) => {
                    eprintln!("已删除 worktree：{name}（分支 worktree-{name} 保留）");
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("删除 worktree 失败：{e}");
                    eprintln!(
                        "退路：先 merge 该分支或清理未提交改动；确要丢弃可 `git worktree remove --force`。"
                    );
                    std::process::exit(1);
                }
            }
        }
        // `attach` 不早退（由 `main` 在进入本函数前单独处理）；此处不可达。
        WorktreeCmd::Attach(_) => {
            unreachable!("Attach 已由 main 提前处理，不应到达 run_worktree_early")
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 模式与 `--resume` 必须在一切之前确定（决定事件消费者 / sink / 会话形态）。
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = parse_args(&argv)?;

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

    // **共享工作区**：进程启动目录，在 `attach` 改写 `config.cwd` **之前**捕获。
    // 会话内 `/resume` 到「无工作区记录」的会话时切回它（`app_loop::apply_resumed_workspace`
    // 的第三种情形）—— 这是「绝不静默采用调用方 cwd」的落点：目标只可能是记录或此值。
    let shared_workspace = config.cwd.clone();

    // `worktree` 子命令分流（**必须早于 skill 扫描**：`attach` 改写 `config.cwd`，
    // 而 skill 根 / 提示词 / 工具三者都从它派生，设计 §1.1/§1.2）。
    //
    // `create` / `list` / `remove` 是进程级动作 ⇒ 在此**执行并早退**，不建 runtime；
    // `attach` 只把工作区换到树上（失败则响亮警告 + 保持共享工作区），随后走正常启动。
    // 捕获的树名 / 树路径**只用于播种**启动期的绑定快照 —— 工作区在运行期可变
    // （会话内 `/resume` 可切树），故后续读取一律走 `WorktreeHandle`（见 `app_loop`）。
    let mut worktree_name: Option<String> = None;
    // 本会话**当前绑定**的工作树路径（显式 attach 或从 meta 恢复时给出；否则共享工作区 ⇒ None）。
    // 回合末据此写 `SessionMeta.workspace`：共享工作区写进去是无用噪声。
    let mut worktree_path: Option<PathBuf> = None;
    // 是否**显式**给了 `worktree attach`（含失败降级）。显式者优先于 meta 记录：
    // 只要用户点名了 attach，就不再套用 meta 里的工作区（哪怕是回退到共享工作区）。
    let explicit_attach = matches!(&args.worktree, Some(WorktreeCmd::Attach(_)));
    match &args.worktree {
        Some(WorktreeCmd::Attach(name)) => {
            match resolve_attach_target(&config.cwd, name.as_deref()) {
                Ok((path, resolved)) => {
                    // 主仓根须在改写 `config.cwd` **之前**取（改后 cwd 已是树根）。
                    let main_root = worktree::main_repo(&config.cwd).ok().map(|r| r.root);
                    worktree_path = Some(path.clone());
                    config.cwd = path;
                    worktree_name = Some(resolved);
                    // 项目级 `.yushan/`（skills / SYSTEM.md）是未跟踪文件，不随 `git worktree`
                    // 迁入 ⇒ 树内不可见。主仓有则响亮告知；无则不打，免噪声。
                    if let Some(root) = main_root
                        && worktree::has_project_config(&root)
                    {
                        eprintln!(
                            "警告：本仓有项目级配置 `.yushan/`（skills / SYSTEM.md），\
                             但它不随 `git worktree` 迁入，本会话中不可见。"
                        );
                    }
                }
                Err(e) => {
                    // 显式降级（设计 §5.3-b）：响亮警告，保持共享工作区继续启动，不早退、不 panic。
                    eprintln!("警告：无法绑定工作树（{e}）");
                    eprintln!("将以共享工作区（{}）继续启动。", config.cwd.display());
                }
            }
        }
        Some(cmd) => run_worktree_early(cmd, &config.cwd),
        None => {}
    }

    // 会话目录 + `--resume` 解析 —— 必须**在模型解析之前**（meta 参与模型选择），
    // 且**在 skill 扫描之前**：meta 记录的工作区要与 `attach` 注入 `config.cwd`
    // **同一时机**生效，而 skill 根 / 提示词 / 工具都从 `config.cwd` 派生。
    let sessions_dir = sessions_dir();
    let (resume_path, resume_meta) = resolve_resume(&args.resume, &sessions_dir);

    // —— `--resume` 的工作区恢复（**代码在 `attach` 之后、skill 扫描之前**）——
    //
    // 铁律（Codex issue #4791 的教训）：`--resume` **必须**优先采用 meta 记录的工作区，
    // **绝不**静默采用调用方的当前 cwd —— 那会扩大可写根。四情形：
    //   · 显式 `attach` 已给定 ⇒ 显式者优先，此处不动作（仅在与记录不同时说明）；
    //   · 有记录且树仍在 ⇒ 恢复到该树，并告知；
    //   · 有记录但树已不在 ⇒ 明确告知后按共享工作区启动（`config.cwd` 不变）；
    //   · 无记录 ⇒ 什么都不做、**不打任何东西**。
    if explicit_attach {
        if let (Some(explicit), Some(recorded)) = (
            worktree_path.as_deref(),
            resume_meta.as_ref().and_then(|m| m.workspace.as_deref()),
        ) && !same_dir(explicit, recorded)
        {
            eprintln!(
                "注意：本会话记录的工作树为 {}，但已按显式 attach 使用 {}。",
                recorded.display(),
                explicit.display()
            );
        }
    } else if let Some(recorded) = resume_meta.as_ref().and_then(|m| m.workspace.clone()) {
        if recorded.is_dir() {
            let name = recorded
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| recorded.display().to_string());
            eprintln!("已恢复到工作树：{name}（{}）", recorded.display());
            config.cwd = recorded.clone();
            worktree_name = Some(name);
            worktree_path = Some(recorded);
        } else {
            eprintln!(
                "记录的工作树已不存在（{}），本会话以共享工作区启动",
                recorded.display()
            );
        }
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

    // 模型解析（优先级，设计 §5.6）：env 完整凭证 > resume meta（provider 须有凭证）
    // > state.json（last-active provider+model）> 第一个存有凭证的 provider。
    // provider 无凭证时**降级而非失败**。整条链抽成**纯函数** `resolve_model_from_meta`
    // —— 启动期与运行期 `/resume` 共用，杜绝两处漂移。
    //
    // runtime 在此**之前**建：下面「模型名缺失 ⇒ 问端点要一个」那一步可能要同步拉一次
    // `/v1/models`。手工建（而非 `#[tokio::main]`）的理由不变 —— TUI 模式的 UI 循环
    // **必须在没有 runtime 上下文的线程上跑**（它用 `blocking_send`）。
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    if let Some(choice) = resolve_model_from_meta(&config, resume_meta.as_ref(), Some(&saved_state))
    {
        config.provider = choice.provider;
        config.api_base = Some(choice.api_base);
        config.api_key = Some(choice.api_key);
        config.model = choice.model;
    }

    // 走到这里**仍没有模型名**、而端点与凭证齐全 ⇒ 同步拉一次该端点的 `/v1/models`，
    // 取**第一条**当起始模型。两种情形落在这里：
    //   · `YUSHAN_API_BASE` + `YUSHAN_API_KEY` 直连、但没给 `YUSHAN_MODEL`；
    //   · auth 里有凭证、但 `model` 是空的（上次 `/login` 时没能读到目录）。
    // 这是启动期**唯一**可能等网络的路径，且只在「用户没点名模型」时发生。
    // 拉不到 ⇒ **明确失败**（提示显式给名字）：名字写死在代码里只会过时，猜一个
    // 不存在/不兼容的模型名更糟。
    if config.model.is_empty() && config.is_configured() {
        let base = config.api_base.clone().unwrap_or_default();
        let key = config.api_key.clone().unwrap_or_default();
        match rt.block_on(provider::ProviderRegistry::fetch_models(&base, &key)) {
            provider::FetchModelsResult::Success(models) => {
                if let Err(reason) = adopt_first_catalog_model(&mut config, &base, models) {
                    return Err(catalog_unavailable(base.as_str(), reason).into());
                }
            }
            provider::FetchModelsResult::AuthError(_) => {
                return Err(catalog_unavailable(base.as_str(), "鉴权失败").into());
            }
            provider::FetchModelsResult::NetworkError(_) => {
                return Err(catalog_unavailable(base.as_str(), "网络不可达").into());
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
    // 这份是**基值**：`fixed_prompt_tokens` 由 [`assembly::AgentFactory::build`] 按每次
    // 工作区派生出的 system prompt 量（见下方装配段），故本值不影响 `context_window`。
    let limits = resolve_limits(&config);

    // 注册 model factory（适配器特有的构建逻辑）
    config.set_model_factory(|cfg| {
        let base = cfg.api_base.as_ref()?;
        let key = cfg.api_key.as_ref()?;
        let compat = cfg.current_compat();
        // `new` 自建 client（`owns_client == true`），故 `with_timeouts` 会按配置重建
        // 带 `connect_timeout` 的 client —— 见适配器 `with_timeouts` 的守卫。
        let retry = cfg.model_retry();
        let timeout = cfg.model_timeout();
        let mut model = OpenAICompatibleModel::new(OpenAICompatibleConfig {
            api_base: base.clone(),
            api_key: key.clone(),
            model: cfg.model.clone(),
            // 不设输出上限：交由 **provider 自身的模型上限**（DeepSeek 单次可达 ~200k）。
            // 主循环的 `ModelRequest.max_tokens` 也是 `None` ⇒ 适配器**整个字段不序列化**，
            // 绝不人为把模型掐断。辅助调用（摘要）在各自的请求里自带上限。
            max_tokens: None,
            temperature: Some(0.7),
            compat,
        })
        .with_retry(RetryConfig {
            max_attempts: retry.max_attempts,
            base_delay_ms: retry.base_delay_ms,
            max_delay_ms: retry.max_delay_ms,
        })
        .with_timeouts(TimeoutConfig {
            connect_secs: timeout.connect_secs,
            idle_secs: timeout.idle_secs,
        })
        // 重试痕迹走 app 的 logging（ADR-0028 决策 7）：对 TUI 不可见、对排障可见。
        // 适配器绝不直接 print —— 这条回调是它唯一的诊断出口。
        .with_logger(std::sync::Arc::new(|msg: &str| crate::logging::log(msg)));
        // 网关接入要求（OpenCode Go 的 `x-opencode-session` 等）：属**传输层**，
        // 由 provider 知识给出，不经 `OpenAICompatibleConfig`（那是方言）。
        for (header, value) in cfg.request_headers() {
            model = model.with_extra_header(&header, &value);
        }
        Some(Box::new(model))
    });

    // 按**模型名**构造（subagent 的 `model` 参数）：同一 provider 上的另一个模型。
    // 未配置凭证 / 名字为空 ⇒ `None` ⇒ 调用方 **fail-closed**（绝不静默回退到父模型）。
    config.set_model_factory_for(|cfg, name| {
        if name.is_empty() {
            return None;
        }
        let base = cfg.api_base.as_ref()?;
        let key = cfg.api_key.as_ref()?;
        let compat = cfg.current_compat();
        let retry = cfg.model_retry();
        let timeout = cfg.model_timeout();
        let mut model = OpenAICompatibleModel::new(OpenAICompatibleConfig {
            api_base: base.clone(),
            api_key: key.clone(),
            model: name.to_string(),
            // 同主工厂：不设输出上限，交由 provider 自身的模型上限。
            max_tokens: None,
            temperature: Some(0.7),
            compat,
        })
        .with_retry(RetryConfig {
            max_attempts: retry.max_attempts,
            base_delay_ms: retry.base_delay_ms,
            max_delay_ms: retry.max_delay_ms,
        })
        .with_timeouts(TimeoutConfig {
            connect_secs: timeout.connect_secs,
            idle_secs: timeout.idle_secs,
        })
        .with_logger(std::sync::Arc::new(|msg: &str| crate::logging::log(msg)));
        for (header, value) in cfg.request_headers() {
            model = model.with_extra_header(&header, &value);
        }
        Some(Box::new(model))
    });

    // ── 装配工厂 ──
    //
    // 工具表 / `working_dir` / system prompt 三者同源于 `workspace`；启动路径与会话内
    // `/resume` 换工作区共用 [`assembly::AgentFactory`] 这一份装配序列（防漂移）。
    // 顺序即设计 §1.2「启动期」：**先探测装工具**（`toolset` 是唯一装配接缝），
    // **再由工具表派生提示词的工具清单** —— 同一份表，清单顺序 == 注册顺序。
    let workspace = config.cwd.clone();
    // web 配置与模型配置**无关**（工具拿不到 `Model` 的正面利用）：`from_env()` 只读
    // env 一次并冻结，**无副作用到「清理」为止**。
    let web = WebConfig::from_env();
    // 清理在此**显式**发生，且**只在启动期一次**：只对 Primary 档。**不进工厂** ——
    // 工厂每次 `build`（含 `/resume` 重建）都会跑，清理塞进去会重复删用户数据。
    sweep_fetched(&web, FETCHED_MAX_AGE);
    let subagents_dir = std::sync::Arc::new(subagent::SubagentsDir::resolve());
    // 产物目录清理：只清 Primary 档的超龄文件（`YUSHAN_SUBAGENTS_DIR` 只写不删）。
    subagents_dir.sweep_old();

    let factory = assembly::AgentFactory::new(
        web,
        catalogue,
        limits,
        config.max_turn_duration(),
        subagents_dir,
    );
    // 一次装配：工具表 / working_dir / system prompt 三者同源于 `workspace`。
    let assembly::BuiltAgent {
        mut agent,
        subagent,
        limits: agent_limits,
        hint: tool_hint,
    } = factory.build(&workspace)?;

    // **启动期提示合并成一条**（多行、绝不互相覆盖）：工具降级 + skill 诊断
    // （遮蔽 / 非法值）+ 自定义 prompt 的代价说明 + R5 的窗口警告。
    // 去处由模式决定：TUI → `Outbound::Output`（transcript）；`-p` / `--json` → stderr。
    let hint = startup_hints(
        tool_hint,
        &skill_diags,
        custom_prompt_note(&workspace, config.skills.len()),
        fixed_cost_note(&agent_limits),
    );

    // 启动期可发现性：默认新会话且目录非空时，提示存在可恢复会话。
    let hint = with_resume_hint(
        hint,
        matches!(args.resume, Resume::New) && sessions_dir_has_entries(&sessions_dir),
    );

    // **工作区绑定快照**：名字 + 路径同源于启动期捕获的树名 / 树路径（attach 或 meta 恢复）。
    // 工作区在运行期可变（会话内 `/resume` 切树），故退出行与回合末 meta 都经此句柄读，
    // 不取固定局部量（见 `app_loop::WorktreeHandle`）。
    let worktree_binding = worktree_name
        .clone()
        .zip(worktree_path.clone())
        .map(|(name, path)| app_loop::WorktreeBinding { name, path });
    let worktree_handle = app_loop::WorktreeHandle::new(worktree_binding);

    match args.mode {
        Mode::Print(task) => {
            // 无 TUI → 启动期降级提示照旧走 stderr（设计 §5 输出三分）。
            if let Some(hint) = &hint {
                eprintln!("{hint}");
            }
            // 与 TUI 的 `Request::Prompt` **同一套**展开：`-p "/git-status"` 也生效。
            // 标题素材（`raw_task`）由 [`raw_and_expanded_task`] 在展开之前留底（C3）。
            let (raw_task, task) = raw_and_expanded_task(task, &config.skills);
            let (sink, rx) =
                ChannelSink::new(channel_capacity(), LifecyclePolicy::StopWhenConsumerGone);
            // 装箱前取读数句柄：装箱后仍能读到终态背压计数。
            let stats_handle = sink.stats_handle();
            let model = config.build_model();
            let mut wiring = rt.block_on(async {
                match resume_path.clone() {
                    Some(p) => Wiring::resume(model, Box::new(sink), sessions_dir.clone(), p).await,
                    None => {
                        Wiring::persistent_new(model, Box::new(sink), sessions_dir.clone()).await
                    }
                }
            })?;
            let result = rt.block_on(run_print_mode(
                &mut agent,
                &mut wiring,
                rx,
                task,
                config.max_turn_duration(),
                &stats_handle,
                args.stats,
                &config,
                Some(&subagent),
            ))?;
            // 单回合模式不换工作区，但退出行 / meta 仍从同一句柄读（与 TUI 路径同源）。
            let binding = worktree_handle.get();
            rt.block_on(persist_single_turn(
                &mut wiring,
                &raw_task,
                config.provider.as_deref(),
                binding.as_ref(),
                &result,
            ))?;
            // 预算到点 ⇒ 非零退出（正常完成路径退出码不变）。
            exit_with_code(single_turn_exit_code(&result));
        }
        Mode::Json(task) => {
            // 同 `-p`：无 TUI → stderr（**绝不能**混进 stdout 的逐行 Envelope）。
            if let Some(hint) = &hint {
                eprintln!("{hint}");
            }
            let (raw_task, task) = raw_and_expanded_task(task, &config.skills);
            let (sink, rx) =
                ChannelSink::new(channel_capacity(), LifecyclePolicy::StopWhenConsumerGone);
            let stats_handle = sink.stats_handle();
            let model = config.build_model();
            let mut wiring = rt.block_on(async {
                match resume_path.clone() {
                    Some(p) => Wiring::resume(model, Box::new(sink), sessions_dir.clone(), p).await,
                    None => {
                        Wiring::persistent_new(model, Box::new(sink), sessions_dir.clone()).await
                    }
                }
            })?;
            let result = rt.block_on(run_json_mode(
                &mut agent,
                &mut wiring,
                rx,
                task,
                config.max_turn_duration(),
                &stats_handle,
                args.stats,
                &config,
                Some(&subagent),
            ))?;
            let binding = worktree_handle.get();
            rt.block_on(persist_single_turn(
                &mut wiring,
                &raw_task,
                config.provider.as_deref(),
                binding.as_ref(),
                &result,
            ))?;
            // 预算到点 ⇒ 非零退出（正常完成路径退出码不变）。
            exit_with_code(single_turn_exit_code(&result));
        }
        Mode::Interactive => {
            // TUI 模式下提示**不进 stderr**，改走 transcript（`Outbound::Output`）——
            // 起屏前写 stderr 的行会落在主屏上，退出 alt-screen 后成为残渣。
            run_interactive(
                rt,
                config,
                state_store,
                agent,
                subagent,
                factory,
                hint,
                sessions_dir,
                resume_path,
                worktree_handle,
                shared_workspace,
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

    use crate::provider::AuthEntry;
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
                resume: Resume::New,
                worktree: None,
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
                resume: Resume::New,
                worktree: None,
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
                resume: Resume::New,
                worktree: None,
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
                resume: Resume::New,
                worktree: None,
            }
        );
        assert_eq!(
            parse_args(&s(&["--json", "--stats", "do", "x"])).unwrap(),
            Args {
                mode: Mode::Json("do x".into()),
                stats: true,
                resume: Resume::New,
                worktree: None,
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
                resume: Resume::New,
                worktree: None,
            }
        );
        // 既有语义不破：`--help` 从来不是 flag（仅首位置判定，且此处非 `--stats`）。
        assert_eq!(
            parse_args(&s(&["-p", "--help", "me"])).unwrap(),
            Args {
                mode: Mode::Print("--help me".into()),
                stats: false,
                resume: Resume::New,
                worktree: None,
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

    /// **解析矩阵**：三模式 × `--resume [<id>]` / `--stats`，含**两种 flag 顺序**；
    /// 任务文本逐字保留；未知 flag 与旧 `=` 形态都报错。
    #[test]
    fn parse_resume_matrix_and_flag_order() {
        // TUI 形态：裸 `--resume` = Pick；`--resume <id>`（空格） = Id。
        assert_eq!(
            parse_args(&s(&["--resume"])).unwrap(),
            Args {
                mode: Mode::Interactive,
                stats: false,
                resume: Resume::Pick,
                worktree: None,
            }
        );
        assert_eq!(
            parse_args(&s(&["--resume", "abc"])).unwrap(),
            Args {
                mode: Mode::Interactive,
                stats: false,
                resume: Resume::Id("abc".into()),
                worktree: None,
            }
        );
        // `=` 形态已移除 ⇒ 报错（不再静默降级）。
        assert!(parse_args(&s(&["--resume=abc"])).is_err());

        // `-p`：`--stats` 与 `--resume <id>` **顺序无关**。
        let a = parse_args(&s(&["-p", "--stats", "--resume", "x", "do", "it"])).unwrap();
        let b = parse_args(&s(&["-p", "--resume", "x", "--stats", "do", "it"])).unwrap();
        assert_eq!(a, b, "两种 flag 顺序应等价");
        assert_eq!(
            a,
            Args {
                mode: Mode::Print("do it".into()),
                stats: true,
                resume: Resume::Id("x".into()),
                worktree: None,
            }
        );

        // `-p --resume <id> <任务>`：id 归 resume，任务文本保留。
        assert_eq!(
            parse_args(&s(&["-p", "--resume", "abc", "task"])).unwrap(),
            Args {
                mode: Mode::Print("task".into()),
                stats: false,
                resume: Resume::Id("abc".into()),
                worktree: None,
            }
        );

        // `--resume <任务>`（其后无 token）：任务文本必需 ⇒ 不当 id，裸 flag + 任务。
        assert_eq!(
            parse_args(&s(&["--json", "--resume", "task"])).unwrap(),
            Args {
                mode: Mode::Json("task".into()),
                stats: false,
                resume: Resume::Pick,
                worktree: None,
            }
        );
        assert_eq!(
            parse_args(&s(&["-p", "--resume", "solo"])).unwrap(),
            Args {
                mode: Mode::Print("solo".into()),
                stats: false,
                resume: Resume::Pick,
                worktree: None,
            }
        );

        // flag 区之后：任务文本逐字保留（`--resume` 落进文本就不再是 flag）。
        assert_eq!(
            parse_args(&s(&["-p", "hi", "--resume=x"])).unwrap(),
            Args {
                mode: Mode::Print("hi --resume=x".into()),
                stats: false,
                resume: Resume::New,
                worktree: None,
            }
        );

        // 拼错的 flag 仍报错。
        assert!(parse_args(&s(&["--nope"])).is_err());
        assert!(parse_args(&s(&["--resum=x"])).is_err());
    }

    /// `worktree` 子命令解析：四动词 + 名参可省 + 未知动词 / 缺名 ⇒ Err。
    #[test]
    fn parse_worktree_subcommands() {
        assert_eq!(
            parse_args(&s(&["worktree", "create", "x"])).unwrap(),
            Args {
                mode: Mode::Interactive,
                stats: false,
                resume: Resume::New,
                worktree: Some(WorktreeCmd::Create(Some("x".into()))),
            }
        );
        assert_eq!(
            parse_args(&s(&["worktree", "create"])).unwrap().worktree,
            Some(WorktreeCmd::Create(None))
        );
        assert_eq!(
            parse_args(&s(&["worktree", "list"])).unwrap().worktree,
            Some(WorktreeCmd::List)
        );
        assert_eq!(
            parse_args(&s(&["worktree", "remove", "y"]))
                .unwrap()
                .worktree,
            Some(WorktreeCmd::Remove("y".into()))
        );
        assert!(parse_args(&s(&["worktree", "explode"])).is_err());
        assert!(parse_args(&s(&["worktree"])).is_err());
        assert!(parse_args(&s(&["worktree", "remove"])).is_err());
        assert!(parse_args(&s(&["worktree", "list", "extra"])).is_err());
    }

    /// `worktree create` 的名参**以 `-` 开头 ⇒ Err**（否则会把 `-p` 当名字建树）；
    /// 省略名字仍表示自动命名。
    ///
    /// 变异：把 `optional_name` 的 `starts_with('-')` 守卫删掉 ⇒ `create -p` 变 `Ok`
    /// （`Some("-p")`）⇒ 本测试红。
    #[test]
    fn parse_worktree_create_rejects_flag_like_name() {
        let err = parse_args(&s(&["worktree", "create", "-p"])).unwrap_err();
        assert!(
            err.contains("不能以 `-` 开头"),
            "以 `-` 开头的名字应被拒并给出提示：{err}"
        );
        assert!(parse_args(&s(&["worktree", "create", "--json"])).is_err());
        // 省略名字 ⇒ 自动命名（不受守卫影响）。
        assert_eq!(
            parse_args(&s(&["worktree", "create"])).unwrap().worktree,
            Some(WorktreeCmd::Create(None))
        );
    }

    /// `attach` 名参可选，且可带 `-p` / `--json` / `--resume [<id>]`（设计 §5.1）。
    #[test]
    fn parse_worktree_attach_modes() {
        assert_eq!(
            parse_args(&s(&["worktree", "attach", "feat"])).unwrap(),
            Args {
                mode: Mode::Interactive,
                stats: false,
                resume: Resume::New,
                worktree: Some(WorktreeCmd::Attach(Some("feat".into()))),
            }
        );
        assert_eq!(
            parse_args(&s(&["worktree", "attach"])).unwrap().worktree,
            Some(WorktreeCmd::Attach(None))
        );
        let p = parse_args(&s(&["worktree", "attach", "feat", "-p", "do it"])).unwrap();
        assert_eq!(p.mode, Mode::Print("do it".into()));
        assert_eq!(p.worktree, Some(WorktreeCmd::Attach(Some("feat".into()))));
        let j = parse_args(&s(&["worktree", "attach", "--json", "--resume", "x", "do"])).unwrap();
        assert_eq!(j.mode, Mode::Json("do".into()));
        assert_eq!(j.resume, Resume::Id("x".into()));
        assert_eq!(j.worktree, Some(WorktreeCmd::Attach(None)));
    }

    /// N11 回归：**非 flag 首 token 的既有静默 Interactive 行为不变**。
    #[test]
    fn parse_non_flag_token_still_interactive() {
        assert_eq!(parse_args(&s(&["whatever"])).unwrap(), new_interactive());
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
                run_single_turn(
                    &mut agent,
                    &mut wiring,
                    AgentInput::text("hi"),
                    Duration::from_secs(60),
                    None,
                    None
                ),
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
            run_single_turn(
                &mut agent,
                &mut wiring,
                AgentInput::text("hi"),
                Duration::from_secs(60),
                None,
                None
            ),
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

    /// 测试模型：每次 `complete` 先停留 [`SlowToolCallModel::DELAY`]，再返回一个
    /// **不存在**的 tool call。
    ///
    /// 停留让 wall-clock 计时器得以在回合**进行中**到点；返回 tool call（而非纯文本）
    /// 让回合跨过一个轮边界，`BasicLoop` 的 abort 探针得以命中计时器 push 的 `Abort`。
    struct SlowToolCallModel;

    impl SlowToolCallModel {
        /// 略长于测试用的 `1s` 预算，保证计时器先到点。
        const DELAY: Duration = Duration::from_millis(1300);

        fn new() -> Self {
            Self
        }
    }

    #[async_trait::async_trait]
    impl ys_model::Model for SlowToolCallModel {
        fn model_id(&self) -> &str {
            "slow-tool-call"
        }

        async fn complete(
            &self,
            _request: ys_model::ModelRequest,
            _sink: &mut dyn ys_model::ModelEventSink,
        ) -> Result<ys_model::ModelResponse, ys_model::ModelError> {
            tokio::time::sleep(Self::DELAY).await;
            Ok(ys_model::ModelResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: ToolCallId("slow-1".into()),
                        name: "noop".into(),
                        arguments: serde_json::Value::Null,
                    }],
                },
                usage: Usage::default(),
                finish_reason: ys_model::FinishReason::ToolCalls,
            })
        }
    }

    /// 13b. **一次性模式的 wall-clock 预算**（`-p` / `--json` 的兜底）：预算到点 ⇒
    ///      回合以 `Cancelled` 收场。
    ///
    /// 预算时长来自 `config.json` 的 `max_turn_duration_secs`（经 `load_user_config`
    /// 加载）。先钉住「取到的正是写入的 `1s`」——与配置值绑定，**不是**写死的常量；
    /// 再断言回合终局。变异：把 `config.max_turn_duration()` 换成写死的 48h 默认值 ⇒
    /// 计时器不会在用例内到点，回合跑满 `max_rounds` 以 `MaxRounds` 收场 ⇒ 本用例变红。
    ///
    /// `max_rounds = 2` 给「无计时器」的情形一个**有界**收场（`MaxRounds`），使误删
    /// 计时器表现为干净的断言失败，而非无限挂起。
    ///
    /// **说明**：到点的说明行走 **stderr**（stdout 承载模型增量 / JSON 流，必须干净），
    /// 进程内无法可靠捕获 ⇒ 说明行的存在性留给进程级 T10 用例；本用例只钉住用户可观察
    /// 的终局 `Cancelled`。
    #[tokio::test]
    async fn budget_exhaustion_cancels_single_turn() {
        // `Config::from_env` 会 `getenv` 凭证 key，须与并发改写 env 的测试串行。
        let mut config = {
            let _guard = crate::test_env::env_lock();
            config::Config::from_env().unwrap()
        };

        let dir = unique_tmp_dir("budget_cancel");
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"max_turn_duration_secs": 1}"#).unwrap();
        config.load_user_config(&path);
        // 不写死时长：断言取到的正是配置里写的 1s（与写入值绑定）。
        assert_eq!(
            config.max_turn_duration(),
            Duration::from_secs(1),
            "预算时长必须来自 config.json（max_turn_duration_secs）"
        );

        let (sink, rx) = ChannelSink::new(
            DEFAULT_CHANNEL_CAPACITY,
            LifecyclePolicy::StopWhenConsumerGone,
        );
        let mut agent = build_agent(
            String::new(),
            PathBuf::from("."),
            Vec::new(),
            ys_component::RunLimits::new(2),
        )
        .unwrap();
        let mut wiring =
            Wiring::ephemeral(Some(Box::new(SlowToolCallModel::new())), Box::new(sink));

        let mut out: Vec<u8> = Vec::new();
        let joined = tokio::time::timeout(Duration::from_secs(8), async {
            tokio::join!(
                run_single_turn(
                    &mut agent,
                    &mut wiring,
                    AgentInput::text("hi"),
                    config.max_turn_duration(),
                    None,
                    None,
                ),
                consume_events(rx, &mut out, write_json_envelope),
            )
        })
        .await;

        let _ = std::fs::remove_dir_all(&dir);

        let (run_result, consume_result) =
            joined.expect("预算到点后回合应结束（预算不生效时回合跑满 max_rounds 收场，非挂起）");
        let result = run_result.expect("turn 应成功返回（Cancelled 是正常 RunResult）");
        consume_result.expect("consume 应成功");
        assert_eq!(
            result.stop_reason,
            StopReason::Cancelled,
            "预算耗尽应以 Cancelled 收场（时长写死为默认 48h ⇒ 本回合 MaxRounds 收场，用例变红）"
        );
    }

    /// 13c. **预算到点 ⇒ 非零退出码**（`-p` / `--json` 的退出契约）。
    ///
    /// 钉住退出码**判定**：`Cancelled` ⇒ 1，正常完成 ⇒ 0。`process::exit` 本身
    /// 只能在进程级验证（留给 T10），但「按什么退出」由 [`single_turn_exit_code`]
    /// 决定，故此处直接覆盖：变异「去掉 `Cancelled` 判定 ⇒ 恒 0」会让第一条断言变红。
    #[test]
    fn single_turn_exit_code_is_nonzero_on_budget_cancellation() {
        let cancelled = RunResult {
            stop_reason: StopReason::Cancelled,
            usage: Usage::default(),
            rounds: 0,
            final_message: None,
        };
        let completed = RunResult {
            stop_reason: StopReason::Completed,
            usage: Usage::default(),
            rounds: 1,
            final_message: None,
        };
        let no_output = RunResult {
            stop_reason: StopReason::NoOutput,
            usage: Usage::default(),
            rounds: 1,
            final_message: None,
        };
        assert_eq!(
            single_turn_exit_code(&cancelled),
            1,
            "预算到点（Cancelled）须非零退出"
        );
        assert_eq!(
            single_turn_exit_code(&completed),
            0,
            "正常完成路径退出码不变（回归护栏）"
        );
        // 空产出**没有可交差的输出**，与预算到点同档 ⇒ 非零退出（不可静默装作成功）。
        // 变异：把 `StopReason::NoOutput` 从匹配臂里删掉 ⇒ 本断言变红。
        assert_eq!(
            single_turn_exit_code(&no_output),
            1,
            "空产出（NoOutput）须非零退出"
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
                "compaction": { "trigger_ratio": 0.5, "keep_ratio": 0.25 },
                "max_rounds": 42
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
        assert_eq!(
            limits.max_rounds, 42,
            "max_rounds 必须来自 config（config.json 的显式值）"
        );
        assert_eq!(
            limits.max_rounds,
            config.max_rounds(),
            "resolve_limits 必须逐值取自 config.max_rounds()"
        );

        // 缺省回落：`config.json` 未给 `max_rounds` ⇒ `u32::MAX`（= 不设）。
        // 断言取与库默认（10）不同的值，避免「恰好等于保守默认」的恒真。
        let dir_default = unique_tmp_dir("resolve_limits_default");
        let path_default = dir_default.join("config.json");
        std::fs::write(
            &path_default,
            r#"{ "context_window": { "resolve-limits-model": 262144 } }"#,
        )
        .unwrap();
        let mut config_default = config::Config::from_env().unwrap();
        config_default.model = "resolve-limits-model".into();
        config_default.load_user_config(&path_default);
        let limits_default = resolve_limits(&config_default);
        assert_eq!(
            limits_default.max_rounds,
            config_default.max_rounds(),
            "缺省时必须取自 config.max_rounds()"
        );
        assert_eq!(limits_default.max_rounds, u32::MAX, "缺省 = 不设");
        assert_ne!(
            limits_default.max_rounds,
            ys_component::RunLimits::default().max_rounds,
            "app 产品默认必须与库的保守默认（10）分离（防恒真）"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir_default);
    }

    /// **N16**：`resolve_limits` 把回合级工具结果预算设为 `context_window / 4`。
    ///
    /// 断言选与默认值不同的窗口（`262_144` ⇒ `65_536`），且预算非零（`0` 会关闭闸门）。
    ///
    /// 变异：把 `limits.max_turn_tool_result_tokens = config.context_window() / 4;` 这行
    /// 删掉 ⇒ 落回默认 `0` ⇒ 变红；把 `/ 4` 改成别的比例 ⇒ 变红。
    #[test]
    fn resolve_limits_sets_turn_budget_from_window() {
        let _guard = crate::test_env::env_lock();

        let dir = unique_tmp_dir("resolve_limits_budget");
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{ "context_window": { "budget-model": 262144 } }"#).unwrap();
        let mut config = config::Config::from_env().unwrap();
        config.model = "budget-model".into();
        config.load_user_config(&path);

        let limits = resolve_limits(&config);
        assert_eq!(limits.context_window, 262_144, "前置：窗口取自 config");
        assert_eq!(
            limits.max_turn_tool_result_tokens, 65_536,
            "回合级预算必须 == context_window / 4"
        );
        assert_ne!(
            limits.max_turn_tool_result_tokens, 0,
            "预算不得为 0（0 = 关闭闸门）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **两组默认常数不许漂移**（跨 crate 守卫）。
    ///
    /// 适配器侧 `RetryConfig::default()` / `TimeoutConfig::default()` 与 app 侧
    /// [`config::ModelRetryConfig::default`] / [`config::ModelTimeoutConfig::default`]
    /// 是同一组常数各写一份，本用例逐字段钉住相等。
    ///
    /// **两侧语义不对称**：生产路径的 model factory（本文件）**总是**显式调用
    /// `.with_retry(cfg.model_retry())` / `.with_timeouts(cfg.model_timeout())`，
    /// 故生效的是 **app 侧常量** —— 改 app 侧默认值有生产后果；改适配器侧的
    /// `RetryConfig::default()` / `TimeoutConfig::default()` 在生产行为上**惰性**
    /// （它们不在任何生产路径上），仅当将来撤掉显式注入、或出现别的直接消费者时
    /// 才显现。本用例的价值是把两组常数锁在一起，堵住「谁才是真源」的漂移。
    ///
    /// 变异：把 app 侧任一默认值改掉（如 `ModelRetryConfig::max_attempts` 5 → 4）⇒
    /// 本用例变红。
    #[test]
    fn adapter_and_app_defaults_do_not_drift() {
        let app_retry = config::ModelRetryConfig::default();
        let adapter_retry = RetryConfig::default();
        assert_eq!(
            app_retry.max_attempts, adapter_retry.max_attempts,
            "model_retry.max_attempts 默认值两边必须一致"
        );
        assert_eq!(
            app_retry.base_delay_ms, adapter_retry.base_delay_ms,
            "model_retry.base_delay_ms 默认值两边必须一致"
        );
        assert_eq!(
            app_retry.max_delay_ms, adapter_retry.max_delay_ms,
            "model_retry.max_delay_ms 默认值两边必须一致"
        );

        let app_timeout = config::ModelTimeoutConfig::default();
        let adapter_timeout = TimeoutConfig::default();
        assert_eq!(
            app_timeout.connect_secs, adapter_timeout.connect_secs,
            "model_timeout.connect_secs 默认值两边必须一致"
        );
        assert_eq!(
            app_timeout.idle_secs, adapter_timeout.idle_secs,
            "model_timeout.idle_secs 默认值两边必须一致"
        );
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
        assert!(
            note.contains(&trigger.to_string()),
            "提示应给出触发点：{note}"
        );
        assert!(note.contains("context_window"), "提示应可操作：{note}");

        // 差一个 token ⇒ 仍装得下 ⇒ 不提示。
        limits.fixed_prompt_tokens = trigger - 1;
        assert!(fixed_cost_note(&limits).is_none(), "未达触发点不该打扰用户");

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

    /// 启动期可发现性提示（`Resume with --resume`）：`triggered` 为假时**原样返回**
    /// （`None→None`、`Some(x)→Some(x)`），为真时追加一行且**原有提示在前**。
    ///
    /// 变异：忽略 `triggered`（恒加提示）⇒ 第三条断言变红。
    #[test]
    fn with_resume_hint_only_appends_when_triggered() {
        assert_eq!(
            with_resume_hint(None, true).as_deref(),
            Some("Resume with --resume")
        );
        assert_eq!(
            with_resume_hint(Some("原有提示".into()), true).as_deref(),
            Some("原有提示\nResume with --resume")
        );
        assert_eq!(with_resume_hint(None, false), None);
        assert_eq!(
            with_resume_hint(Some("原有提示".into()), false).as_deref(),
            Some("原有提示"),
            "未触发必须原样返回，不得追加"
        );
    }

    /// 退出提示行（[`session_exit_line`]）：会话文件已落盘 ⇒ `session: <id>`
    /// （id = 文件 stem）；未落盘 / 无路径 ⇒ `None`。
    ///
    /// 变异：去掉 `is_file` 守卫 ⇒ 惰性新会话也会打出一个 `--resume` 找不到的 id，
    /// 第一条与第三条断言变红。
    #[test]
    fn session_exit_line_requires_persisted_file() {
        let dir = unique_tmp_dir("session_exit_line");
        let path = dir.join("2026-10-10-abc.jsonl");
        assert_eq!(session_exit_line(Some(&path)), None, "未落盘不得打 id");
        assert_eq!(session_exit_line(None), None, "一次性会话无文件");

        std::fs::write(&path, "{}\n").unwrap();
        assert_eq!(
            session_exit_line(Some(&path)).as_deref(),
            Some("session: 2026-10-10-abc"),
            "id 取文件 stem"
        );

        // 目录（而非文件）同名时不算落盘：`is_file` 而非 `exists`。
        let as_dir = dir.join("2026-10-10-dir.jsonl");
        std::fs::create_dir_all(&as_dir).unwrap();
        assert_eq!(session_exit_line(Some(&as_dir)), None);
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
        assert!(
            note.contains("/skill-name"),
            "提示应指出确定性路线仍可用：{note}"
        );

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
        assert!(
            expanded.contains("<skill name=\"git-status\""),
            "{expanded}"
        );
        assert!(expanded.contains("正文第一行"), "{expanded}");
        assert!(
            expanded.contains("现在改了什么"),
            "参数原样保留：{expanded}"
        );

        let plain = "修一下这个 bug".to_string();
        assert_eq!(
            expand_task(plain.clone(), &entries),
            plain,
            "非 skill 的 task 必须逐字节不变"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `finalize_turn_meta`：首行为 meta、title 折叠空白取展开前原文、provider/model
    /// 与 tokens 落盘、title 幂等。
    #[tokio::test]
    async fn finalize_turn_meta_writes_title_once_and_refreshes_stats() {
        let dir = unique_tmp_dir("finalize");
        let mut wiring =
            Wiring::persistent_new(None, Box::new(ys_event::CollectingSink::new()), dir.clone())
                .await
                .unwrap();
        let mut stats = status::TurnStats::default();
        stats.record(&Usage {
            input_tokens: 11,
            output_tokens: 4,
            ..Default::default()
        });

        finalize_turn_meta(
            &mut wiring,
            Some("  第一条   用户消息  "),
            Some("deepseek"),
            Some("deepseek-chat"),
            None,
            Some(Path::new("/tmp/yushan-tree")),
            &stats,
        )
        .await
        .unwrap();

        let path = wiring.session_path().unwrap().to_path_buf();
        let content = std::fs::read_to_string(&path).unwrap();
        // 首行必须是 meta（不是裸 Message）。
        let meta: ys_session::SessionMeta =
            serde_json::from_str(content.lines().next().unwrap()).unwrap();
        assert_eq!(meta.title.as_deref(), Some("第一条 用户消息"), "折叠空白");
        assert_eq!(meta.provider.as_deref(), Some("deepseek"));
        assert_eq!(meta.model.as_deref(), Some("deepseek-chat"));
        assert_eq!(meta.input_tokens, 11);
        assert_eq!(meta.output_tokens, 4);
        assert_eq!(meta.turns, 1);
        assert_eq!(
            meta.workspace.as_deref(),
            Some(Path::new("/tmp/yushan-tree")),
            "attach 状态应把工作树路径写进 meta"
        );

        // 第二次：title 幂等，tokens / turns 刷新；workspace 传 `None`（共享工作区）
        // **不写**，故已落盘的记录**不得被清空**（与 provider/model 同语义）。
        stats.record(&Usage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        });
        finalize_turn_meta(
            &mut wiring,
            Some("第二条"),
            Some("deepseek"),
            Some("deepseek-chat"),
            None,
            None,
            &stats,
        )
        .await
        .unwrap();
        let meta = wiring.meta().unwrap();
        assert_eq!(
            meta.title.as_deref(),
            Some("第一条 用户消息"),
            "title 只写一次"
        );
        assert_eq!(meta.turns, 2);
        assert_eq!(meta.input_tokens, 12);
        assert_eq!(
            meta.workspace.as_deref(),
            Some(Path::new("/tmp/yushan-tree")),
            "workspace 传 None 不得清空已记录的工作树"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `finalize_turn_meta`：`provider` / `model` **仅 `Some` 时写** —— 后一次调用的
    /// `None` 不得清空前一次落盘的 provider / model（env 凭证路径下 D10 第 2 级恢复的
    /// 前提），而 tokens / turns 照常刷新。
    ///
    /// 变异：把 `if let Some` 改回无条件赋值 ⇒ 末两条 provider / model 断言变红
    /// （被清成 null，resume 再也恢复不出该会话的 model）。
    #[tokio::test]
    async fn finalize_turn_meta_keeps_provider_model_when_none() {
        let dir = unique_tmp_dir("finalize_keep");
        let mut wiring =
            Wiring::persistent_new(None, Box::new(ys_event::CollectingSink::new()), dir.clone())
                .await
                .unwrap();

        let mut stats = status::TurnStats::default();
        stats.record(&Usage {
            input_tokens: 3,
            output_tokens: 2,
            ..Default::default()
        });
        finalize_turn_meta(
            &mut wiring,
            Some("第一条"),
            Some("deepseek"),
            Some("m1"),
            None,
            None,
            &stats,
        )
        .await
        .unwrap();
        assert_eq!(wiring.meta().unwrap().provider.as_deref(), Some("deepseek"));
        assert_eq!(wiring.meta().unwrap().model.as_deref(), Some("m1"));

        // 第二次：provider / model 为 `None`（env 凭证路径下 `config.provider` 恒为
        // `None`）—— 不得清空；tokens / turns 仍刷新。
        stats.record(&Usage {
            input_tokens: 4,
            output_tokens: 1,
            ..Default::default()
        });
        finalize_turn_meta(&mut wiring, Some("第二条"), None, None, None, None, &stats)
            .await
            .unwrap();

        let meta = wiring.meta().unwrap();
        assert_eq!(
            meta.provider.as_deref(),
            Some("deepseek"),
            "None 不得清空已落盘的 provider"
        );
        assert_eq!(
            meta.model.as_deref(),
            Some("m1"),
            "None 不得清空已落盘的 model"
        );
        assert_eq!(meta.input_tokens, 7, "tokens 照常刷新");
        assert_eq!(meta.turns, 2, "turns 照常刷新");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `raw_and_expanded_task`（`-p` / `--json` 共用）：`expanded` 是带 `<skill …>`
    /// 信封的正文，`raw` 必须**等于展开前原文**且不含 `<skill` —— 它是
    /// [`finalize_turn_meta`] 的标题素材（C3），落盘标题里也不可能出现信封。
    ///
    /// 变异：把函数体里的 `raw` 改成 `expanded.clone()` ⇒ 第一条断言变红。
    #[test]
    fn raw_and_expanded_task_keeps_raw_free_of_skill_envelope() {
        let dir = unique_tmp_dir("raw_expand");
        let skill_dir = dir.join("git-status");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: git-status\ndescription: 状态\n---\n正文第一行\n",
        )
        .unwrap();
        let (entries, diags) = crate::skills::scan(&[dir.clone()]);
        assert_eq!(entries.len(), 1, "测试前提：{diags:?}");

        let original = "/git-status 现在改了什么".to_string();
        let (raw, expanded) = raw_and_expanded_task(original.clone(), &entries);
        assert!(
            expanded.contains("<skill name=\"git-status\""),
            "展开用于送给模型：{expanded}"
        );
        assert_eq!(raw, original, "标题素材必须是展开前原文");
        assert!(!raw.contains("<skill"), "标题素材不得含信封：{raw}");
        assert!(
            !sessions::derive_stored_title(&raw).contains("<skill"),
            "落盘标题里也不得出现信封"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `persist_single_turn`（`-p`/`--json` 收尾路径）：落盘首行为 meta，title 取
    /// **展开前**原文，model 来自接线器、tokens 来自 `RunResult.usage`。
    #[tokio::test]
    async fn persist_single_turn_writes_meta_with_title_model_tokens() {
        let dir = unique_tmp_dir("persist_single");
        let mut wiring = Wiring::persistent_new(
            Some(Box::new(MockModel::new("harness-model"))),
            Box::new(ys_event::CollectingSink::new()),
            dir.clone(),
        )
        .await
        .unwrap();

        let result = RunResult {
            stop_reason: StopReason::Completed,
            usage: Usage {
                input_tokens: 7,
                output_tokens: 5,
                ..Default::default()
            },
            rounds: 1,
            final_message: None,
        };

        persist_single_turn(&mut wiring, "展开前的原文", Some("deepseek"), None, &result)
            .await
            .unwrap();

        let path = wiring.session_path().unwrap().to_path_buf();
        let content = std::fs::read_to_string(&path).unwrap();
        let meta: ys_session::SessionMeta =
            serde_json::from_str(content.lines().next().unwrap()).unwrap();
        assert_eq!(meta.title.as_deref(), Some("展开前的原文"));
        assert_eq!(meta.provider.as_deref(), Some("deepseek"));
        assert_eq!(meta.model.as_deref(), Some("harness-model"));
        assert_eq!(meta.input_tokens, 7);
        assert_eq!(meta.output_tokens, 5);
        assert_eq!(meta.turns, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ───────────── 运行期 `/resume`：模型解析纯函数 + 启动期顺序（T10 / N4） ─────────────

    /// 造一份**未配置**（无 env 凭证）的 `Config`，auth 落临时目录。
    fn unconfigured_config(tag: &str) -> (config::Config, PathBuf) {
        let dir = unique_tmp_dir(tag);
        let mut cfg = config::Config::from_env().unwrap();
        cfg.api_base = None;
        cfg.api_key = None;
        cfg.provider = None;
        cfg.model = "cfg-default-model".into();
        cfg.registry.set_auth_override(dir.join("auth.json"));
        (cfg, dir)
    }

    fn auth(api_base: &str, api_key: &str, model: &str) -> AuthEntry {
        AuthEntry {
            api_base: api_base.into(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    fn meta_with(provider: Option<&str>, model: Option<&str>) -> ys_session::SessionMeta {
        let mut m = ys_session::SessionMeta::new(0);
        m.provider = provider.map(str::to_string);
        m.model = model.map(str::to_string);
        m
    }

    /// **T10 —— 四档优先级链**：env 完整凭证 > meta（provider 须 `auth_for` 命中）
    /// > state.json > 首个有凭证者；provider 无凭证时**降级而非失败**。
    ///
    /// 与 `apps/coding-agent/tests/session_cli.rs` 组 3 的五例同源（那里钉的是子进程
    /// 实发的 `"model"`）—— 本函数是它们的**纯函数版**。
    #[test]
    fn resolve_model_from_meta_follows_priority_chain() {
        let _guard = crate::test_env::env_lock();

        // ---- 档 1：env 完整凭证 ⇒ 原样取 cfg（即便 meta 指向别的 provider）。----
        let (mut cfg, dir) = unconfigured_config("rmfm_env");
        cfg.api_base = Some("http://env-base/v1".into());
        cfg.api_key = Some("sk-env".into());
        cfg.model = "env-model".into();
        cfg.registry
            .save_auth("minimax", &auth("http://minimax/v1", "sk-mm", "mm-default"))
            .unwrap();
        let choice = resolve_model_from_meta(
            &cfg,
            Some(&meta_with(Some("minimax"), Some("meta-model"))),
            None,
        )
        .expect("env 档应有结果");
        assert_eq!(choice.api_base, "http://env-base/v1");
        assert_eq!(choice.api_key, "sk-env");
        assert_eq!(choice.model, "env-model", "env 凭证必须压过 meta");
        let _ = std::fs::remove_dir_all(&dir);

        // ---- 档 2：meta 的 provider + model（provider 须 auth_for 命中）。----
        let (mut cfg, dir) = unconfigured_config("rmfm_meta");
        cfg.registry
            .save_auth("minimax", &auth("http://minimax/v1", "sk-mm", "mm-default"))
            .unwrap();
        let choice = resolve_model_from_meta(
            &cfg,
            Some(&meta_with(Some("minimax"), Some("meta-model"))),
            None,
        )
        .expect("meta 档");
        assert_eq!(choice.provider.as_deref(), Some("minimax"));
        assert_eq!(choice.api_base, "http://minimax/v1");
        assert_eq!(choice.api_key, "sk-mm");
        assert_eq!(choice.model, "meta-model");

        // 档 2b：meta.model 为 None ⇒ 回落 entry.model。
        let choice =
            resolve_model_from_meta(&cfg, Some(&meta_with(Some("minimax"), None)), None).unwrap();
        assert_eq!(
            choice.model, "mm-default",
            "meta.model 为空应回落 entry.model"
        );

        // 档 2c：meta 的 provider **无凭证** ⇒ 降级（不失败）到下一档。
        cfg.registry
            .save_auth("deepseek", &auth("http://ds/v1", "sk-ds", "ds-default"))
            .unwrap();
        // 无 state ⇒ 落到档 4（首个有凭证者，注册序 deepseek 在前）。
        let choice = resolve_model_from_meta(
            &cfg,
            Some(&meta_with(Some("ghost"), Some("ghost-model"))),
            None,
        )
        .expect("降级到档 4");
        assert_eq!(
            choice.provider.as_deref(),
            Some("deepseek"),
            "无凭证的 meta provider 应降级，不得启用"
        );
        assert_ne!(choice.model, "ghost-model");
        let _ = std::fs::remove_dir_all(&dir);

        // ---- 档 3：state.json 的 last-active。----
        let (mut cfg, dir) = unconfigured_config("rmfm_state");
        cfg.registry
            .save_auth("deepseek", &auth("http://ds/v1", "sk-ds", "ds-default"))
            .unwrap();
        cfg.registry
            .save_auth("minimax", &auth("http://mm/v1", "sk-mm", "mm-default"))
            .unwrap();
        let state = crate::state::AppState {
            last_active_provider: Some("minimax".into()),
            last_active_model: Some("state-model".into()),
        };
        let choice = resolve_model_from_meta(&cfg, None, Some(&state)).expect("state 档");
        assert_eq!(choice.provider.as_deref(), Some("minimax"));
        assert_eq!(choice.model, "state-model");

        // 档 3 的 provider 是**未知** provider ⇒ 降级到档 4。
        let unknown = crate::state::AppState {
            last_active_provider: Some("nope".into()),
            last_active_model: Some("nope-model".into()),
        };
        let choice = resolve_model_from_meta(&cfg, None, Some(&unknown)).expect("降级到档 4");
        assert_eq!(
            choice.provider.as_deref(),
            Some("deepseek"),
            "未知 provider 应降级到首个有凭证者"
        );
        assert_ne!(choice.model, "nope-model");

        // 档 4：无 meta / 无 state ⇒ 首个有凭证者（注册序 deepseek 在前）。
        let choice = resolve_model_from_meta(&cfg, None, None).expect("档 4");
        assert_eq!(choice.provider.as_deref(), Some("deepseek"));
        assert_eq!(choice.model, "ds-default");
        let _ = std::fs::remove_dir_all(&dir);

        // ---- 全部耗尽：无任何凭证 ⇒ None（降级到耗尽，不 panic、不 Err）。----
        let (cfg, dir) = unconfigured_config("rmfm_none");
        assert!(
            resolve_model_from_meta(&cfg, None, None).is_none(),
            "无凭证 ⇒ None"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **N4（I10）—— 启动期出站顺序**：`--resume` 的回放（`SessionResumed`）必须排在
    /// 启动降级提示**之前** —— 反序会让回放触发的 UI 清屏把提示吃掉。
    ///
    /// 变异：把 `startup_outbound_sequence` 的两个 push 对调 ⇒ 本用例第一条断言变红。
    #[test]
    fn startup_outbound_sequence_puts_replay_before_hint() {
        let replay = Outbound::<CodingView>::SessionResumed {
            summary: ys_protocol::SessionSummary {
                id: "sess-1".into(),
                path: PathBuf::from("/tmp/sess-1.jsonl"),
                mtime: SystemTime::UNIX_EPOCH,
                title: "标题".into(),
            },
            thinking_visible: true,
            history: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
            }],
        };

        let seq = startup_outbound_sequence(Some(replay), Some("降级提示".into()));
        assert_eq!(seq.len(), 2);
        assert!(
            matches!(seq[0], Outbound::SessionResumed { .. }),
            "回放必须排在最前（I10）：{seq:?}"
        );
        assert!(
            matches!(&seq[1], Outbound::Output(t) if t == "降级提示"),
            "启动提示排在回放之后：{seq:?}"
        );

        // 无回放时只剩提示；无提示时只剩回放。
        assert!(matches!(
            startup_outbound_sequence(None, Some("h".into())).as_slice(),
            [Outbound::Output(_)]
        ));
        assert!(matches!(
            startup_outbound_sequence(Some(Outbound::Quit), None).as_slice(),
            [Outbound::Quit]
        ));
    }

    /// `resume_replay` 的**构造本身**（区别于 `startup_outbound_sequence` 只管顺序）：
    /// 带 `--resume`（`resuming = true`）时**真的**从接线器已恢复的会话造出
    /// `SessionResumed` —— `history` 非空、`thinking_visible` 取自目标 meta；不带时
    /// 恒 `None`（不回放）。既有用例只喂**手工构造**的 replay，不证「带 `--resume`
    /// 时 replay 真的会被构造出来」。
    ///
    /// 变异：删掉 `if !resuming { return None; }` ⇒ 第二条断言变红；把 `history` 改成
    /// `Vec::new()` ⇒ 第一条的 `history.len()` 断言变红。
    #[tokio::test]
    async fn resume_replay_builds_session_resumed_only_when_resuming() {
        let dir = unique_tmp_dir("resume_replay");
        let path = dir.join("sess-replay.jsonl");

        // 手写一份含 meta + 两条消息的会话文件（`Wiring::resume` 读回）。
        let mut meta = ys_session::SessionMeta::new(0);
        meta.title = Some("回放目标".into());
        meta.thinking_visible = true;
        let mut content = format!("{}\n", serde_json::to_string(&meta).unwrap());
        for m in [
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "旧问题".into(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "旧回答".into(),
                }],
            },
        ] {
            content.push_str(&format!("{}\n", serde_json::to_string(&m).unwrap()));
        }
        std::fs::write(&path, content).unwrap();

        let wiring = Wiring::resume(
            None,
            Box::new(ys_event::CollectingSink::new()),
            dir.clone(),
            path.clone(),
        )
        .await
        .unwrap();

        // 带 `--resume` ⇒ 构造出回放：history 非空、thinking 取自 meta、id/标题正确。
        match resume_replay(&wiring, true) {
            Some(Outbound::SessionResumed {
                summary,
                thinking_visible,
                history,
            }) => {
                assert_eq!(summary.id, "sess-replay");
                assert_eq!(summary.title, "回放目标");
                assert!(thinking_visible, "thinking 取自目标 meta");
                assert_eq!(history.len(), 2, "history 应含会话全部消息：{history:?}");
            }
            other => panic!("带 --resume 必须构造 SessionResumed：{other:?}"),
        }

        // 不带 `--resume` ⇒ 不回放。
        assert!(
            resume_replay(&wiring, false).is_none(),
            "未带 --resume 不得构造回放"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **模型名没有内置默认值**：缺失时把目录第一条当起始模型 —— 并**同时**把目录
    /// 并入缓存，于是窗口解析的 ② 级（厂商自己在目录里报的 `context_window`）立刻生效。
    ///
    /// 变异：取 `models.last()`、或只写 `config.model` 而不 `apply_model_cache`
    /// ⇒ 下面两条断言分别变红。
    #[test]
    fn adopt_first_catalog_model_takes_first_and_seeds_catalog() {
        let _guard = crate::test_env::env_lock();
        let mut cfg = config::Config::from_env().unwrap();
        cfg.model = String::new();

        let models = vec![
            provider::ApiModel {
                id: "deepseek-flash".into(),
                owned_by: Some("deepseek".into()),
                name: None,
                context_window: Some(1_048_576),
            },
            provider::ApiModel {
                id: "deepseek-v4-pro".into(),
                owned_by: Some("deepseek".into()),
                name: None,
                context_window: Some(1_048_576),
            },
        ];
        adopt_first_catalog_model(&mut cfg, "https://api.deepseek.com/", models).unwrap();

        assert_eq!(cfg.model, "deepseek-flash", "起始模型 = 目录第一条");
        assert_eq!(
            cfg.context_window_for("deepseek-flash"),
            1_048_576,
            "窗口取自目录（② 级），不再落到 128K 兜底"
        );
        assert_eq!(
            cfg.registry.models_for_base("https://api.deepseek.com"),
            vec!["deepseek-flash".to_string(), "deepseek-v4-pro".to_string()],
            "目录同时进缓存（`/model` 候选）—— 尾斜杠归一"
        );
    }

    /// 空目录 ⇒ `Err` 且**什么都不改**：宁可失败，也不猜一个可能不存在的模型名。
    #[test]
    fn adopt_first_catalog_model_refuses_empty_catalog() {
        let _guard = crate::test_env::env_lock();
        let mut cfg = config::Config::from_env().unwrap();
        cfg.model = String::new();

        assert!(adopt_first_catalog_model(&mut cfg, "https://x/v1", Vec::new()).is_err());
        assert!(cfg.model.is_empty(), "不得写入任何模型名");
        assert!(cfg.registry.models_for_base("https://x/v1").is_empty());
    }
}
