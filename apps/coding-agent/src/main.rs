mod ansi;
mod channel;
mod commands;
mod config;
mod format;
mod prompt;
mod provider;
mod state;
mod status;
mod view;

#[cfg(feature = "tui-ratatui")]
mod ui;

use std::io::{self, Write};
use std::path::PathBuf;

use tokio::sync::mpsc;

use ys_channel::{Envelope, Inbox, Intent, LifecyclePolicy};
use ys_event::{AgentEvent, EventSink, NoopEventSink};
use ys_loop::AgentInput;
use ys_model::Model;
use ys_model_openai_compat::{OpenAICompatibleConfig, OpenAICompatibleModel};
use ys_runtime::{Agent, AgentBuilder, BuildError};
use ys_session::MemorySession;
use ys_tools_basic::{BashTool, EditTool, ReadTool, WriteTool};

use channel::ChannelSink;

/// 运行模式。
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    /// 单次 print 模式：边生成边打印文本增量。
    Print(String),
    /// JSON 事件模式：逐行序列化 [`Envelope`]。
    Json(String),
    /// 交互式 TUI（本轮不改，仍用 [`NoopEventSink`]）。
    Interactive,
}

/// 解析后的命令行参数。
#[derive(Debug, PartialEq, Eq)]
struct Args {
    mode: Mode,
}

/// 事件出口选择（与消费方式绑定）。抽成纯函数以便单测断言：
/// `Interactive` 必须用 [`NoopEventSink`]（不消费事件），
/// `Print`/`Json` 才用有界信道 [`ChannelSink`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SinkChoice {
    /// 有界信道 + 并发消费（`-p` / `--json`）。
    Channel,
    /// 静默丢弃（交互 TUI；事件桥接不在本轮范围）。
    Noop,
}

/// 模式 → 事件出口。这是「先定模式 → 选消费者 → 建 agent」的判据。
fn sink_choice(mode: &Mode) -> SinkChoice {
    match mode {
        Mode::Print(_) | Mode::Json(_) => SinkChoice::Channel,
        Mode::Interactive => SinkChoice::Noop,
    }
}

/// 解析 argv（不含 argv[0]）。
///
/// **只在首个参数位置判定 flag**：任务文本本身可能以 `-` 开头，
/// 例如 `-p --help me` 的任务文本就是 `--help me`。
fn parse_args(argv: &[String]) -> Result<Args, String> {
    let Some(first) = argv.first().map(String::as_str) else {
        return Ok(Args {
            mode: Mode::Interactive,
        });
    };

    // `-p` / `--json` 之后全部参数 join 成任务文本（保持与旧实现一致）。
    let task = || argv[1..].join(" ");

    match first {
        "-p" => {
            let task = task();
            if task.is_empty() {
                Err("-p 需要一个任务参数，例如：ys-coding-agent -p \"修复这个 bug\"".into())
            } else {
                Ok(Args {
                    mode: Mode::Print(task),
                })
            }
        }
        "--json" => {
            let task = task();
            if task.is_empty() {
                Err("--json 需要一个任务参数，例如：ys-coding-agent --json \"修复这个 bug\"".into())
            } else {
                Ok(Args {
                    mode: Mode::Json(task),
                })
            }
        }
        // 未知 flag：仅当它出现在**首个**位置时才算 flag（其余位置属任务文本）。
        // 单独的 `-` 视为普通参数（约定：读 stdin 的占位，本轮不支持）。
        f if f.starts_with('-') && f != "-" => {
            Err(format!("未知参数：{f}（可用：-p <任务> | --json <任务>）"))
        }
        // 无 flag：交互式（与旧实现一致）。
        _ => Ok(Args {
            mode: Mode::Interactive,
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

/// 组装 agent。sink 由调用方按模式选定 —— 这是「先定模式 → 选消费者 → 建 agent」
/// 的落点：agent 构建发生在模式判定之后。
fn build_agent<M: Model + 'static>(
    model: Option<M>,
    system_prompt: String,
    workspace: PathBuf,
    events: impl EventSink + 'static,
) -> Result<Agent, BuildError> {
    let mut builder = AgentBuilder::new()
        .tool(ReadTool::new(workspace.clone()))
        .tool(WriteTool::new(workspace.clone()))
        .tool(EditTool::new(workspace.clone()))
        .tool(BashTool::new(workspace.clone()))
        .system_prompt(system_prompt)
        .working_dir(workspace.clone(), workspace)
        .approval(ys_tool::AutoApprove)
        .session(MemorySession::new())
        .events(events);

    if let Some(model) = model {
        builder = builder.model(model);
    }

    builder.build()
}

/// print / json 模式的前置检查：无 model 时给出可操作提示并退出。
fn ensure_configured(agent: &Agent) -> Result<(), Box<dyn std::error::Error>> {
    if agent.is_configured() {
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

/// `-p` 模式：并发消费事件流，把文本增量实时写到 stdout。
///
/// 收尾补一个换行，与旧实现（`println!` final_message）的输出保持一致。
async fn run_print_mode(
    agent: &mut Agent,
    rx: mpsc::Receiver<Envelope>,
    task: String,
) -> Result<(), Box<dyn std::error::Error>> {
    ensure_configured(agent)?;

    // 走自转接口 `Agent::run`（而非 `run_turn`）：它每回合调 `begin_turn(n)`，
    // 使 `Envelope.turn` 从 1 起递增。一次用户输入 = 一个 followUp 回合。
    let inbox = Inbox::new();
    inbox.push(AgentInput::text(task.as_str()).message, Intent::FollowUp);

    // 消费与 run 必须**并发**：有界信道若无并发消费者，agent 撞满即等待 → 死锁。
    let (run_result, consume_result) =
        tokio::join!(agent.run(&inbox), consume_print_events(rx, io::stdout()),);

    run_result?;
    let printed_any = consume_result?;

    if printed_any {
        println!();
    }
    Ok(())
}

/// `--json` 模式：并发消费事件流，逐行输出 `Envelope` 的 JSON。
async fn run_json_mode(
    agent: &mut Agent,
    rx: mpsc::Receiver<Envelope>,
    task: String,
) -> Result<(), Box<dyn std::error::Error>> {
    ensure_configured(agent)?;

    // 同 `-p`：走 `Agent::run` 以获得逐回合的 `turn`（从 1 起递增）。
    let inbox = Inbox::new();
    inbox.push(AgentInput::text(task.as_str()).message, Intent::FollowUp);

    let (run_result, consume_result) = tokio::join!(
        agent.run(&inbox),
        consume_events(rx, io::stdout(), write_json_envelope),
    );

    run_result?;
    consume_result?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = config::Config::from_env()?;

    // 从 auth.json 加载已保存的凭证
    config.registry.load_auth();

    // 从 state.json 加载已保存的会话状态
    let mut state_store = state::StateStore::new();
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

    // 构建 command registry
    let command_registry = commands::build_registry();

    // 模式必须在建 agent 之前确定：它决定事件消费者与 sink 的选择。
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = parse_args(&argv)?;

    // 构建 system prompt
    let cwd = config.cwd.clone();
    let system_prompt = prompt::build_system_prompt(&cwd);

    // 构建 model（可选——agent 可在无 API 凭证时启动）
    let model: Option<OpenAICompatibleModel> = if config.is_configured() {
        let compat = config.current_compat();
        Some(OpenAICompatibleModel::new(OpenAICompatibleConfig {
            api_base: config.api_base.clone().unwrap(),
            api_key: config.api_key.clone().unwrap(),
            model: config.model.clone(),
            max_tokens: Some(4096),
            temperature: Some(0.7),
            compat,
        }))
    } else {
        None
    };

    // 为工具构建 workspace
    let workspace = config.cwd.clone();

    // 「先定模式 → 选消费者 → 建 agent」：sink 选择由模式决定。
    let choice = sink_choice(&args.mode);
    match (args.mode, choice) {
        (Mode::Print(task), SinkChoice::Channel) => {
            let (sink, rx) =
                ChannelSink::new(channel_capacity(), LifecyclePolicy::StopWhenConsumerGone);
            let mut agent = build_agent(model, system_prompt, workspace, sink)?;
            run_print_mode(&mut agent, rx, task).await?;
        }
        (Mode::Json(task), SinkChoice::Channel) => {
            let (sink, rx) =
                ChannelSink::new(channel_capacity(), LifecyclePolicy::StopWhenConsumerGone);
            let mut agent = build_agent(model, system_prompt, workspace, sink)?;
            run_json_mode(&mut agent, rx, task).await?;
        }
        (Mode::Interactive, SinkChoice::Noop) => {
            // 交互模式——仅 ratatui（tui-stdout 路径已在 c 阶段删除）。
            // 事件出口仍为 NoopEventSink（TUI 事件桥接不在本轮范围）。
            let mut agent = build_agent(model, system_prompt, workspace, NoopEventSink)?;
            let mut stats = status::TurnStats::default();

            #[cfg(feature = "tui-ratatui")]
            {
                ui::run(
                    &mut agent,
                    &mut config,
                    &command_registry,
                    &mut stats,
                    &mut state_store,
                )
                .await?;
            }
            #[cfg(not(feature = "tui-ratatui"))]
            {
                return Err(
                    "ratatui mode required for interactive TUI; build with --features tui-ratatui"
                        .into(),
                );
            }
        }
        _ => unreachable!("sink_choice 与 mode 必须一一对应"),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use ys_channel::Source;
    use ys_core::{ContentBlock, Message, Role, StopReason, ToolCall, ToolCallId, Usage};

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
                mode: Mode::Print("hello".into())
            }
        );
    }

    /// 2. `--json <任务...>` → Json，多参数 join。
    #[test]
    fn parse_json_joins_remaining_args() {
        assert_eq!(
            parse_args(&s(&["--json", "do", "x"])).unwrap(),
            Args {
                mode: Mode::Json("do x".into())
            }
        );
    }

    /// 3. 无 flag → Interactive。
    #[test]
    fn parse_no_flag_is_interactive() {
        assert_eq!(
            parse_args(&[]).unwrap(),
            Args {
                mode: Mode::Interactive
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
    #[tokio::test]
    async fn run_turn_and_consume_do_not_deadlock_at_small_capacity() {
        use std::path::PathBuf;

        use ys_component::{RunLimits, RuntimeContext};
        use ys_core::CancelToken;
        use ys_loop::{AgentLoop, BasicLoop};
        use ys_model::MockModel;
        use ys_session::MemorySession;
        use ys_tool::ToolRegistry;

        let model = MockModel::new("m");
        model.push_text("hello world");

        let (mut sink, rx) = ChannelSink::new(2, LifecyclePolicy::StopWhenConsumerGone);
        let registry = ToolRegistry::build(vec![]).unwrap();
        let mut session = MemorySession::new();
        let cancel = CancelToken::new();
        let limits = RunLimits::new(5);
        let mut ctx = RuntimeContext::new(
            &model,
            &registry,
            &mut session,
            &mut sink,
            &cancel,
            limits,
            PathBuf::from("."),
            PathBuf::from("."),
            None,
            None,
        );

        let mut out: Vec<u8> = Vec::new();
        let joined = tokio::time::timeout(Duration::from_secs(5), async {
            let run = BasicLoop.run_turn(AgentInput::text("hi"), &mut ctx);
            tokio::join!(run, consume_events(rx, &mut out, write_json_envelope))
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

    /// 10. **M5**：`Interactive` 模式必须用 `NoopEventSink`（不消费事件）；
    ///     `Print`/`Json` 才用有界信道 `ChannelSink`。
    #[test]
    fn interactive_mode_uses_noop_sink() {
        assert_eq!(sink_choice(&Mode::Interactive), SinkChoice::Noop);
        assert_eq!(sink_choice(&Mode::Print("x".into())), SinkChoice::Channel);
        assert_eq!(sink_choice(&Mode::Json("x".into())), SinkChoice::Channel);
    }

    /// 11. 容量 clamp：过小容量被抬到下限；正常容量不动。
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
    ///     说明：当前模型适配器 `stream: false`，每次响应只发 **1 个** delta，
    ///     故生产中的 `-p` 目前只会写出一个增量。本测试**手工投递多个 delta**，
    ///     模拟「流式接通」后的多增量场景；真实多 delta 要等流式接通（后续任务）
    ///     才在生产中走此路径。消费走**生产路径** `consume_print_events`
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

    /// 12. **迁移步 4 回归**：`-p`/`--json` 走 `Agent::run(&inbox)`（而非 `run_turn`），
    ///     `begin_turn(1)` 被驱动 → 信道上所有 `Envelope.turn` 从 **1** 起（不再恒为 0）。
    ///
    ///     修复前：`run_print_mode`/`run_json_mode` 调 `run_turn`，`begin_turn` 从不触发，
    ///     `ChannelSink.turn` 保持初值 0 → 断言失败。
    #[tokio::test]
    async fn run_via_inbox_sets_turn_from_one() {
        use ys_model::MockModel;

        let model = MockModel::new("m");
        model.push_text("hello");

        let (sink, rx) = ChannelSink::new(
            DEFAULT_CHANNEL_CAPACITY,
            LifecyclePolicy::StopWhenConsumerGone,
        );
        let mut agent = build_agent(Some(model), String::new(), PathBuf::from("."), sink).unwrap();

        // 与 `-p`/`--json` 相同的构造：一次用户输入作为 followUp 入队。
        let inbox = Inbox::new();
        inbox.push(AgentInput::text("hi").message, Intent::FollowUp);

        let mut out: Vec<u8> = Vec::new();
        let (run_result, consume_result) = tokio::join!(
            agent.run(&inbox),
            consume_events(rx, &mut out, write_json_envelope),
        );
        run_result.expect("run 应成功");
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
