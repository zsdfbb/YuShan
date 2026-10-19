//! app 侧循环（设计 §7）。
//!
//! 它是「UI 线程」与「agent 线程」之间的那一半 —— 一个线程上跑两层循环：
//!
//! ```text
//! loop {                            ① 回合之间：等一条 Request
//!   let req = request_rx.recv().await?;
//!   match req {
//!     Prompt(msg) => {
//!       raw = user_text(msg);       // 标题素材：skill 展开之前（仅首回合）
//!       msg = expand_skill(msg);    // /skill-name 展开（仅首回合，复核文案不展开）
//!       pending = Some((msg, Initial));
//!       while let Some((input, kind)) = pending.take() {   // 主回合 + 至多一次复核
//!         turns += 1;
//!         let res = {                 ② 回合之内：turn 与事件转发交错
//!           let ports = wiring.ports();
//!           ports.events.begin_turn(turns);
//!           let boundary = QueueBoundarySource::new();
//!           let turn = agent.run_turn(input, AgentPorts::new(.., Some(&boundary)));
//!           loop { select! {
//!             r   = &mut turn            => break r,       // 回合结束
//!             Some(env) = sink_rx.recv() => out.send(Event(env)).await?,  // 事件转发
//!             Some(b) = boundary_rx.recv() => boundary.push(b),            // 轮边界
//!           }}
//!         };
//!         stats.record(&res.usage);
//!         finalize_turn_meta(..).await;   // 回合末写穿 meta
//!       }
//!     SetThinking{visible} => { thinking_visible = visible; }   // 进程级镜像，无输出
//!     SetEffort/SetModel/Login/Logout/NewSession/Compact/Export => { 能力函数 → Output }
//!     ListSessions/ResumeSession => { 会话列表 / 原子恢复 → SessionList/SessionResumed }
//!   }
//!   emit_view();                     // 每个请求处理完刷一次快照
//! }
//! ```
//!
//! # 四个要点
//!
//! **① `ports` 的借用必须包在块里**：`wiring.ports()` 可变借用 `wiring`，且被
//! `turn` future 持有整个回合。若写在 `match` 臂里跨到外层循环，外层就再也不能
//! 碰 `wiring`（`SetModel` / `NewSession` 都要碰）。`{ … }` 让借用随块结束。
//!
//! **② 事件经 app 转发（而非 UI 直读事件信道）**：`Outbound<V>` 是 UI 唯一的
//! 输入流 —— **单一出站写者**。UI 若同时直读事件信道，事件与 `Output` 的先后
//! 就没了保证（两条信道各排各的）。代价是每事件多一跳，可忽略。
//!
//! **③ 一次性 BoundarySource 每回合新建**：`Abort` 会**永久**置位 `is_aborted`，
//! 复用同一个源会让「上一回合的取消」把之后每个回合立刻取消。
//!
//! **④ 每次用户输入恰好跑一个回合**（ADR-0041 撤回 ADR-0027 决策 2 的完工复核）：
//! 回合结束即请求结束，没有追问「真的完工了吗」的第二个回合。回合前的边界排空仍在
//! `run_turn_once` 开头 —— 它挡的是**上一个请求**残留的 `Abort`，与回合数无关。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::mpsc;
use ys_core::{ContentBlock, Message, Role, StopReason};
use ys_event::AgentEvent;
use ys_loop::{AgentInput, LoopError, RunResult};
use ys_protocol::{Boundary, Envelope, Outbound, QueueBoundarySource, Request};
use ys_runtime::{Agent, AgentPorts};
use ys_tui_coding::CodingView;

use crate::capabilities;
use crate::config::Config;
use crate::debug_log::{ArgsMode, DebugSink, debug_path_for};
use crate::logging;
use crate::provider::{FetchModelsResult, ModelCache, ProviderRegistry};
use crate::state::StateStore;
use crate::status::TurnStats;
use crate::view::build_view;
use crate::wiring::Wiring;

/// 会话文件路径的**进程内快照**：app 循环写、`main` 在 UI 退出后读。
///
/// # 为什么不能靠 app 线程收摊后再取
///
/// UI 退出**不等于** app 线程已结束：回合可能正卡在模型调用里，
/// `run_interactive` 只能宽限一段时间，到点连 runtime 一起丢。若等 app 线程
/// 收摊才取路径，最常见的情形（长回合中途退出）恰好取不到。
///
/// # 写者与播种
///
/// 启动态由**调用方播种**（此刻的会话路径归构造 `Wiring` 的一方知道）；
/// `/new` 之后由 app 循环刷新。读取失败（锁中毒）按「无会话」处理 —— 只影响
/// 退出提示行，不影响任何执行路径。
#[derive(Clone, Debug, Default)]
pub struct SessionPathHandle {
    path: Arc<Mutex<Option<PathBuf>>>,
}

impl SessionPathHandle {
    /// 以当前会话路径播种（一次性会话给 `None`）。
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path: Arc::new(Mutex::new(path)),
        }
    }

    /// 刷新快照（换会话后调用）。
    fn record(&self, path: Option<&Path>) {
        if let Ok(mut slot) = self.path.lock() {
            *slot = path.map(Path::to_path_buf);
        }
    }

    /// 当前快照。
    pub fn get(&self) -> Option<PathBuf> {
        self.path.lock().ok().and_then(|slot| slot.clone())
    }
}

/// 本会话**当前绑定**的工作树：名字（退出行用）+ 路径（回合末写 meta 用）。
///
/// 名字与路径成对出现在同一次绑定里，故合成一个值：分裂成两个 `Option` 会出现
/// 「有名字没路径」这类不可能状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeBinding {
    /// 树名（退出行 `worktree: <name>`）。
    pub name: String,
    /// 树绝对路径（回合末写 `SessionMeta.workspace`）。
    pub path: PathBuf,
}

impl WorktreeBinding {
    /// 由路径派生名字（`file_name`；无则退回整条路径的显示形态）。
    pub fn from_path(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Self { name, path }
    }
}

/// 工作区绑定的**跨生命周期快照**：`main` 启动期播种、app 循环在 `/resume` 后刷新、
/// `main` 在 `rt.shutdown_timeout` 之后读（退出行）。
///
/// 形态照 [`SessionPathHandle`]：工作区在**运行期可变**（会话内 `/resume` 可切到另一棵
/// 树的会话），故退出行不能由启动期的固定局部量给出。
///
/// `None` = 共享工作区（未绑定任何树）。读取失败（锁中毒）按「无绑定」处理 —— 只影响
/// 退出行与回合末 meta 的 `workspace` 字段，不影响任何执行路径。
#[derive(Clone, Debug, Default)]
pub struct WorktreeHandle {
    inner: Arc<Mutex<Option<WorktreeBinding>>>,
}

impl WorktreeHandle {
    /// 以当前绑定播种（共享工作区给 `None`）。
    pub fn new(binding: Option<WorktreeBinding>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(binding)),
        }
    }

    /// 刷新快照（切换工作区后调用）。
    fn record(&self, binding: Option<WorktreeBinding>) {
        if let Ok(mut slot) = self.inner.lock() {
            *slot = binding;
        }
    }

    /// 当前绑定（`None` = 共享工作区）。
    pub fn get(&self) -> Option<WorktreeBinding> {
        self.inner.lock().ok().and_then(|slot| slot.clone())
    }
}

/// app 循环的失败：只有「UI 侧已经收摊」这一种（`Outbound` 信道关闭）。
///
/// 模型 / 工具失败不是循环的失败 —— `BasicLoop` 已发 `RunFailed`（UI 显示 Error
/// 行），循环继续等下一条 `Request`。
pub type AppLoopError = Box<dyn std::error::Error + Send + Sync>;

/// 主循环的一次唤醒：一条 UI 请求，或后台模型目录回来了。
///
/// 把两个来源收成一个枚举，是为了在 `select!` 结束后**先释放对 `models_rx` 的
/// 借用**再改写它（收到目录即置 `None`、停用该分支）。
enum Wake {
    Request(Option<Request>),
    Models(Option<crate::provider::ModelCache>),
}

/// 跑 app 侧循环，直到 UI 侧把所有发送端 drop 掉（= 正常收摊）。
///
/// 参数里的三条信道方向见设计 §8：`request_rx` / `boundary_rx` 是 UI → app，
/// `out_tx` 是 app → UI，`sink_rx` 是 `ChannelSink` → app（app 再转成
/// `Outbound::Event`）。
#[allow(clippy::too_many_arguments)]
pub async fn run(
    mut agent: Agent,
    mut wiring: Wiring,
    mut config: Config,
    state_store: StateStore,
    mut stats: TurnStats,
    mut session_started: Instant,
    mut request_rx: mpsc::Receiver<Request>,
    mut boundary_rx: mpsc::Receiver<Boundary>,
    out_tx: mpsc::Sender<Outbound<CodingView>>,
    mut sink_rx: mpsc::Receiver<Envelope>,
    // 后台模型目录信道：`main` 在起屏后并发拉 `/v1/models`，结果**一次性**投回
    // （见下方循环开头的 `select!`）。`None` = 起屏时无已登录 provider，无需拉取。
    mut models_rx: Option<mpsc::Receiver<crate::provider::ModelCache>>,
    mut subagent: Option<crate::subagent::SubagentSetup>,
    // 装配工厂：会话内 `/resume` 换工作区时按新工作区**重建** agent（工具表 +
    // `working_dir` + system prompt 三者一起换）。
    factory: crate::assembly::AgentFactory,
    // 共享工作区（进程启动目录）：`/resume` 到**无工作区记录**的会话时切回它。
    default_workspace: PathBuf,
    session_path: SessionPathHandle,
    // 本会话当前绑定的工作树（`None` = 共享工作区）。**运行期可变**（`/resume` 刷新）：
    // 回合末据此写 `SessionMeta.workspace`，`main` 退出后据此打退出行。
    worktree: WorktreeHandle,
    // 结构化执行日志（`--debug`）。`None` = 关闭（零文件、零线程）。
    mut debug: Option<DebugSink>,
    // 适配器尝试读数 → 当前 debug 文件；`/new` / `/resume` 后重新绑定。
    model_trace: Option<crate::debug_log::ModelTraceRouter>,
    // `run_start` 的工具元数据（启动装配期量好；工具表不随 `/new` / `/resume` 变）。
    debug_tools: Vec<(String, usize)>,
) -> Result<(), AppLoopError> {
    // 回合号从 1 起（`Envelope.turn` 靠 `begin_turn` 携带）。
    let mut turns: u32 = 0;
    // 本 run 累计模型轮数（`run_end.rounds`；失败回合取事件流累计）。
    let mut run_rounds: u32 = 0;
    // debug 记录的会话 id（`/new` / `/resume` 后刷新）与本文件内的 run 序号
    // （启动期 `main` 已写 run=0 的 `run_start`）。
    let mut debug_session_id = crate::session_id_for(wiring.session_path());
    let mut debug_run: u64 = 0;

    // `/thinking` 显示开关的**进程级缓存**：启动时由 meta 初始化，`Request::SetThinking`
    // 单向更新，回合末落 meta。UI 的 `App.thinking_visible` 才是运行期真值，本缓存只
    // 作持久化镜像；**`/new` 不重置它**（展示偏好随进程，设计 §5.6）。
    let mut thinking_visible = wiring.meta().map(|m| m.thinking_visible).unwrap_or(false);

    loop {
        // 等一条 UI 请求；后台模型目录若尚未返回，则与之赛跑。目录**一次性**到达，
        // 收到后把 `models_rx` 置 `None`，此后主循环只等 `Request`。
        //
        // 目录到手 ⇒ 并入 registry、刷新一次视图：`/model` 候选此刻才拿到这两家的
        // 模型名（渲染路径依旧一次 HTTP 都不发，写与读同在这个线程）。
        let wake = match models_rx.as_mut() {
            Some(rx) => tokio::select! {
                req = request_rx.recv() => Wake::Request(req),
                cache = rx.recv() => Wake::Models(cache),
            },
            None => Wake::Request(request_rx.recv().await),
        };
        let req = match wake {
            Wake::Request(Some(req)) => req,
            Wake::Request(None) => {
                // UI 侧 drop 了发送端 → 正常退出前排空 debug 队列并写 `run_end`。
                flush_debug(&mut debug, &stats, run_rounds, 0);
                return Ok(());
            }
            Wake::Models(cache) => {
                models_rx = None; // 目录只投一次；此后不再参与 `select!`
                if let Some(cache) = cache {
                    config.registry.apply_model_cache(cache);
                    let view = build_view(
                        &config,
                        &agent,
                        &wiring,
                        &config.registry,
                        &state_store,
                        &stats,
                        session_started,
                    );
                    send_or_flush(&out_tx, Outbound::View(view), &mut debug, &stats, run_rounds)
                        .await?;
                }
                continue;
            }
        };

        let mut lines: Vec<String> = Vec::new();
        // `/resume` 的会话出站（`SessionList` / `SessionResumed`）：**排在 `Output`
        // 之前**是固定不变量（I2）—— 反了的话，UI 侧回放的清屏会把通知吃掉。
        let mut session_event: Option<Outbound<CodingView>> = None;

        match req {
            Request::Prompt(message) => {
                // **标题素材取 skill 展开之前的用户原文**（C3）—— 展开后的正文是
                // `<skill …>` 信封，不能当标题。
                let raw_user_text = crate::sessions::user_text(&message);
                // **`/skill-name` 展开**（唯一的 skill 接线点，设计 §选择理由 B）：
                // 复用 `Request::Prompt` 而**不加协议变体** —— 展开型命令与纯文本输入
                // 走**同一条**回合启动路径（`run_turn_once` 内部的 `turns += 1` /
                // `set_context_window` / `run_turn` 一字未动）。位置必须在
                // `run_turn_once` 之前：展开后的正文才是本回合真正的输入。
                // **只作用于用户原文** —— 复核消息由 `should_review` 直接产出、不再展开。
                let message = expand_skill_invocation(message, &config.skills);

                // 一个 `Request::Prompt` **恰好跑一个回合**（ADR-0041 撤回了完工复核）。
                //
                // **wall-clock 预算在此算一次**：`deadline` 覆盖整个 `pending` 循环
                // —— 主回合与复核回合共享同一个截止点，时长只来自
                // `config.max_turn_duration()`。
                let request_started = Instant::now();
                let deadline = request_started + config.max_turn_duration();
                // 本请求一份子 usage 累加器（跨主回合 + 复核回合）。
                let subagent_totals = Arc::new(crate::subagent::UsageTotals::default());
                let mut ctx = TurnCtx {
                    agent: &mut agent,
                    wiring: &mut wiring,
                    config: &config,
                    turns: &mut turns,
                    stats: &mut stats,
                    thinking_visible,
                    deadline,
                    request_started,
                    cumulative_rounds: 0,
                    run_rounds: &mut run_rounds,
                    out_tx: &out_tx,
                    sink_rx: &mut sink_rx,
                    boundary_rx: &mut boundary_rx,
                    subagent: subagent.as_ref(),
                    subagent_totals,
                    debug: debug.as_mut(),
                    debug_session_id: &debug_session_id,
                };
                // **一个 `Request::Prompt` 恰好跑一个回合** —— 没有完工复核
                // （ADR-0041 撤回 ADR-0027 决策 2）。
                // 外层 `Err` = UI 已收摊（出站失败）：先收尾 debug 再退出；
                // 内层 `TurnOutcome` 的 `Err` 只是本回合模型/工具失败，app 继续。
                match run_turn_once(&mut ctx, AgentInput::new(message)).await {
                    Ok(outcome) => match outcome {
                        Ok(result) => {
                            ctx.stats.record(&result.usage);
                            // 回合末写穿 meta（title/model/tokens/thinking，三模式共用）。
                            let model = ctx.wiring.model_id().map(str::to_string);
                            // 工作区从**跨生命周期句柄**读（`/resume` 可改），不取启动期局部量。
                            let ws = worktree.get();
                            if let Err(e) = crate::finalize_turn_meta(
                                ctx.wiring,
                                raw_user_text.as_deref(),
                                ctx.config.provider.as_deref(),
                                model.as_deref(),
                                Some(ctx.thinking_visible),
                                // TUI 模式：档位随回合末写穿会话 meta。
                                Some(ctx.config.effort),
                                ws.as_ref().map(|b| b.path.as_path()),
                                ctx.stats,
                            )
                            .await
                            {
                                logging::log(&format!("failed to persist session meta: {e}"));
                            }
                        }
                            // `BasicLoop` 在返回错误前已发 `RunFailed` —— UI 已经看到
                            // Error 行，这里不再往 transcript 重复刷一条，只落日志。
                            // 回合失败**不向上传播**：请求到此结束，app 循环继续等下一个。
                            Err(e) => logging::log(&format!("turn {} failed: {e}", ctx.turns)),
                    },
                    Err(e) => {
                        let rounds = *ctx.run_rounds;
                        let usage = ctx.stats.cumulative_usage();
                        if let Some(sink) = ctx.debug.as_deref_mut() {
                            sink.flush_run(crate::debug_log::RunEndMeta {
                                stop_reason: "RunFailed",
                                rounds,
                                exit_code: 1,
                                usage: Some(&usage),
                            });
                        }
                        return Err(e);
                    }
                }
            }

            Request::SetThinking { visible } => {
                // 单向更新进程级镜像；UI 侧已本地即时生效。无输出。
                thinking_visible = visible;
            }

            Request::SetEffort { effort } => {
                // 设置照常保存 + 重建 model；不支持的 provider 由能力函数给出 warning。
                lines = capabilities::set_effort(&mut config, &mut wiring, effort);
            }

            Request::SetModel { model, provider } => {
                lines = capabilities::set_model(
                    &mut config,
                    &mut wiring,
                    &state_store,
                    &model,
                    provider.as_deref(),
                );
            }

            Request::Login {
                provider,
                api_key,
                api_base,
            } => {
                // 起始模型只能从端点来：先按 `/login` 的同一规则定出 base，再拉它的
                // `/v1/models`，把目录交给 `capabilities::login` 定模型。
                //
                // 这是用户显式动作 ⇒ 这次网络值得等（`fetch_models` 自带 5s 超时）；
                // 拉到的目录同时进 registry 缓存，`/model` 候选与窗口查询立刻可用。
                match capabilities::resolve_login_base(&config, &provider, api_base.as_deref()) {
                    Err(msg) => lines = msg,
                    Ok(base) => {
                        // Esc 可打断这次拉取：命令不过 `BasicLoop`，没人读 `Boundary`，
                        // 不在这里 `select!` 一个取消口，就只剩「Ctrl-C 关整个进程」一条出路。
                        // 取消 = **什么也不改**（与浮层里 Esc 的契约一致）。
                        match cancel_on_abort(
                            ProviderRegistry::fetch_models(&base, &api_key),
                            &mut boundary_rx,
                        )
                        .await
                        {
                            None => {
                                logging::log(&format!("login {base}: cancelled by user (Esc)"));
                                lines = vec![
                                    "Login cancelled. Nothing was changed.".to_string(),
                                ];
                            }
                            Some(fetched) => {
                                let catalog = match fetched {
                                    FetchModelsResult::Success(models) if !models.is_empty() => {
                                        Some(models)
                                    }
                                    FetchModelsResult::Success(_) => {
                                        logging::log(&format!("login {base}: empty model list"));
                                        None
                                    }
                                    FetchModelsResult::AuthError(e) => {
                                        logging::log(&format!("login {base}: auth error: {e}"));
                                        None
                                    }
                                    FetchModelsResult::NetworkError(e) => {
                                        logging::log(&format!("login {base}: network error: {e}"));
                                        None
                                    }
                                };
                                lines = capabilities::login(
                                    &mut config,
                                    &mut wiring,
                                    &state_store,
                                    &provider,
                                    &api_key,
                                    api_base.as_deref(),
                                    catalog.as_deref(),
                                );
                                if let Some(models) = catalog {
                                    let mut cache = ModelCache::new();
                                    cache.insert(base.trim_end_matches('/').to_string(), models);
                                    config.registry.apply_model_cache(cache);
                                }
                            }
                        }
                    }
                }
            }

            Request::Logout => {
                lines = capabilities::logout(&mut config, &mut wiring, &state_store);
            }

            Request::NewSession => {
                lines = capabilities::new_session(&mut wiring).await;
                // 新会话 = 新的会话起点（状态行的时长据此重置）。
                session_started = Instant::now();
                // 换会话也换了文件 —— 刷新给 `main` 的退出提示快照。
                session_path.record(wiring.session_path());
                // `/new` **切新 debug 文件**（旧文件自然结束），并开一个新的 run。
                if let Some(sink) = debug.as_mut() {
                    if let Some(path) = debug_path_for(wiring.session_path()) {
                        sink.switch_file(&path);
                    }
                    debug_run += 1;
                    sink.begin_run(debug_run, crate::unix_now_ms());
                    debug_session_id = crate::session_id_for(wiring.session_path());
                    if let Some(trace) = &model_trace {
                        trace.attach(sink.handle(), debug_session_id.clone());
                    }
                    crate::emit_run_start(
                        sink,
                        &config,
                        wiring.session_path(),
                        &debug_session_id,
                        &debug_tools,
                    );
                }
            }

            Request::Compact => {
                // 窗口与策略都取自 `config` —— 与 agent 每回合用的那份同源
                // （`/compact` 是用户显式触发的压缩，不等自动触发阈值）。
                let policy = config.compact_policy();
                lines = capabilities::compact(&mut wiring, &policy, config.context_window()).await;
            }

            Request::Export { path } => {
                lines = capabilities::export(&wiring, path);
            }

            Request::ListSessions => {
                match wiring.sessions_dir_path() {
                    // I13：内存会话不可恢复 —— **只**提示，不发列表（不弹浮层）。
                    None => {
                        lines = vec![
                            "Cannot list sessions: this session is not persisted.".to_string(),
                        ];
                    }
                    // I4：**空列表也要发** —— UI 据此提示「没有可恢复的会话」。
                    Some(dir) => {
                        session_event =
                            Some(Outbound::SessionList(capabilities::list_sessions(dir)));
                    }
                }
            }

            Request::ResumeSession { target } => {
                // `/resume` 前后的会话路径：相同 ⇒ 追加同一文件；不同 ⇒ 切文件。
                let path_before = wiring.session_path().map(Path::to_path_buf);
                let (out_lines, resumed) =
                    capabilities::resume_session(&mut config, &mut wiring, &state_store, &target)
                        .await;
                lines = out_lines;
                if let Some(r) = resumed {
                    // 提交步的 app 侧一半（§3.3 第 5 步；此后不得再有失败路径）。
                    //
                    // **I9：thinking 三方真值必须一次对齐** —— `thinking_visible` 是本
                    // 进程的持久化镜像，**每回合末无条件写进 meta**（见下方 `Prompt` 臂）。
                    // 只改 UI 的真值而漏了它 ⇒ 切换后**第一个回合**就把**旧会话**的
                    // thinking 写进**新会话** meta。
                    thinking_visible = r.thinking_visible;
                    // 决策 5：`TurnStats` 由目标 meta 重新播种（「等于重启打开该会话」）。
                    // `turns`（`Envelope.turn` 的驱动）是「本**进程**回合序」，**不归零**
                    // —— 两者语义不同，刻意分叉。
                    stats = crate::seed_stats(&wiring);
                    session_started = Instant::now();
                    // **I3：会话路径快照必须刷新** —— 否则退出提示仍指向旧会话。
                    session_path.record(wiring.session_path());
                    // **工作区**：按目标 meta 记录重建 agent（工具表 + working_dir +
                    // system prompt 一起换），并刷新工作区句柄。四情形见 helper 文档。
                    if let Some(msg) = apply_resumed_workspace(
                        &factory,
                        &mut agent,
                        &mut config,
                        &mut subagent,
                        &worktree,
                        &default_workspace,
                        r.workspace.as_deref(),
                    ) {
                        lines.push(msg);
                    }
                    // 同一 session ⇒ 追加同一文件（只开新 run）；跨 session ⇒ 切到
                    // 目标会话的 debug 文件。两种情况都以一条新的 `run_start` 开头。
                    if let Some(sink) = debug.as_mut() {
                        let path_after = wiring.session_path().map(Path::to_path_buf);
                        if path_after != path_before
                            && let Some(path) = debug_path_for(path_after.as_deref())
                        {
                            sink.switch_file(&path);
                        }
                        debug_run += 1;
                        sink.begin_run(debug_run, crate::unix_now_ms());
                        debug_session_id = crate::session_id_for(path_after.as_deref());
                        if let Some(trace) = &model_trace {
                            trace.attach(sink.handle(), debug_session_id.clone());
                        }
                        crate::emit_run_start(
                            sink,
                            &config,
                            path_after.as_deref(),
                            &debug_session_id,
                            &debug_tools,
                        );
                    }
                    session_event = Some(Outbound::SessionResumed {
                        summary: r.summary,
                        thinking_visible: r.thinking_visible,
                        history: r.history,
                    });
                }
            }
        }

        // 会话出站必须抢在 `Output` 之前（I2）——`SessionResumed` 的清屏会吃掉
        // 其后紧跟的提示，故它排在最前。
        if let Some(msg) = session_event {
            send_or_flush(&out_tx, msg, &mut debug, &stats, run_rounds).await?;
        }

        // 命令输出走 transcript（设计 §5：命令期间的诊断 → `Outbound::Output`）。
        if !lines.is_empty() {
            send_or_flush(
                &out_tx,
                Outbound::Output(lines.join("\n")),
                &mut debug,
                &stats,
                run_rounds,
            )
            .await?;
        }

        // 每个请求处理完刷新快照 —— 状态行（provider / model / tokens）才不会滞后。
        let view = build_view(
            &config,
            &agent,
            &wiring,
            &config.registry,
            &state_store,
            &stats,
            session_started,
        );
        send_or_flush(&out_tx, Outbound::View(view), &mut debug, &stats, run_rounds).await?;
    }
}

/// 命令内部的可取消等待 —— 把 `Esc` 的 `Boundary::Abort` 接到一条**不属于回合**的
/// future 上。
///
/// # 为何需要
///
/// 命令请求不经过 `BasicLoop`，没有轮边界去读 `Boundary`。`/login` 却要在命令里做
/// 一次网络往返（`/v1/models`，`fetch_models` 自带 5s 超时，TCP 挂起时更久）：
/// 这段时间里 Esc 没人读，用户唯一的出路是 Ctrl-C 关掉整个进程。
///
/// # 语义
///
/// - 先完成的赢 —— `Some(fut 的结果)`，与不用本函数逐字等价；
/// - `Abort` 先到 —— `None`，**future 被 drop**（请求真正取消），调用方必须
///   遵守「取消不改任何状态」；
/// - `Steer` 等与命令无关的边界消息直接丢弃：命令不是回合，没有轮边界可以装它；
///   命令结束后下一回合的起点也会把它排空（`run_turn_once` 头部的排空），
///   故丢弃不改变任何已有语义；
/// - 边界信道关闭（UI 没了）—— 不再有取消入口，交 future 自己收场。
async fn cancel_on_abort<F: std::future::Future>(
    fut: F,
    boundary_rx: &mut mpsc::Receiver<Boundary>,
) -> Option<F::Output> {
    let mut fut = std::pin::pin!(fut);
    loop {
        tokio::select! {
            out = &mut fut => return Some(out),
            msg = boundary_rx.recv() => match msg {
                Some(Boundary::Abort) => return None,
                Some(_) => continue,
                None => return Some(fut.await),
            },
        }
    }
}

/// 会话内 `/resume` 之后，按目标 meta 的 `workspace` 决定是否**重建 agent** 切工作区。
///
/// 返回要附加到 `/resume` 输出里的一行（`None` = 无需/维持现状，不打）。重建成功后
/// **工具表 / `working_dir` / system prompt / subagent** 一起换，`config.cwd` 同步刷新
/// （UI 的 `/status` 从 `CodingView.cwd` 读到新值），工作区句柄刷新（回合末 meta 与退出行同源）。
///
/// # 四情形（对齐启动期 U3 的语义，但**绝不静默采用调用方 cwd**）
///
/// - **记录存在且树仍在** ⇒ 与当前**不同**才重建（相同则不动，不打扰用户）；
/// - **记录存在但树已不在** ⇒ 明确告知、**保持当前工作区**（不重建）；
/// - **无记录**且当前在工作树 ⇒ 切回共享工作区（重建），并告知；
/// - **无记录**且当前已在共享工作区 ⇒ 不动。
///
/// 铁律（Codex issue #4791 的教训）：目标工作区**只**来自 meta 记录或共享工作区，
/// 绝不回退成调用方 cwd —— 那会静默扩大可写根。
fn apply_resumed_workspace(
    factory: &crate::assembly::AgentFactory,
    agent: &mut Agent,
    config: &mut Config,
    subagent: &mut Option<crate::subagent::SubagentSetup>,
    handle: &WorktreeHandle,
    default_workspace: &Path,
    recorded: Option<&Path>,
) -> Option<String> {
    let current = handle.get();
    let current_path = current
        .as_ref()
        .map(|b| b.path.clone())
        .unwrap_or_else(|| default_workspace.to_path_buf());

    // 目标绑定：`Some` = 记录的工作树（须存在）；`None` = 共享工作区。
    let target: Option<WorktreeBinding> = match recorded {
        Some(p) if p.is_dir() => Some(WorktreeBinding::from_path(p.to_path_buf())),
        // 记录了但树已不在 ⇒ 告知并保持当前工作区，**不重建**。
        Some(p) => {
            return Some(format!(
                "记录的工作树已不存在（{}），保持当前工作区不变",
                p.display()
            ));
        }
        None => None,
    };
    let target_path = target
        .as_ref()
        .map(|b| b.path.clone())
        .unwrap_or_else(|| default_workspace.to_path_buf());

    if crate::same_dir(&current_path, &target_path) {
        return None; // 已在目标工作区（含「都在共享工作区」）⇒ 不重建。
    }

    match factory.build(&target_path) {
        Ok(built) => {
            *agent = built.agent;
            *subagent = Some(built.subagent);
            config.cwd = target_path;
            let message = match &target {
                Some(b) => format!("已恢复到工作树：{}（{}）", b.name, b.path.display()),
                None => format!("已切回共享工作区（{}）", default_workspace.display()),
            };
            handle.record(target);
            Some(message)
        }
        Err(e) => Some(format!("警告：无法切换到工作区（{e}），保持当前工作区不变")),
    }
}

/// 跑一个回合所需的运行时句柄（一次 `Request::Prompt` 一份，跨该请求的回合链复用）。
///
/// 收进结构体只是为把 `run_turn_once` 的形参收敛；各字段的**类型与所有权不改**。
/// `wiring` / `turns` / `stats` 跨回合可变，其余只读。回合末的记账 / 写穿 meta
/// 由外层循环用同一份 ctx 完成（`stats` / `wiring` / `config` / `thinking_visible`
/// 因此留在结构体里）。
struct TurnCtx<'a> {
    agent: &'a mut Agent,
    wiring: &'a mut Wiring,
    config: &'a Config,
    turns: &'a mut u32,
    stats: &'a mut TurnStats,
    thinking_visible: bool,
    /// 本请求（含复核回合）的 wall-clock 截止点 —— 在 `Request::Prompt` 入口算
    /// **一次**，主回合与复核回合共享同一个值（`Instant` 是 `Copy`，跨回合不变）。
    /// 时长只来自 [`Config::max_turn_duration`]，**绝不写死**。
    deadline: Instant,
    /// 本请求的起始时刻（= 预算计时的原点）—— 供 §4.9 可观测行的 `elapsed` 用。
    request_started: Instant,
    /// 本请求**累计**的模型调用轮数（主回合 + 复核回合的 `RunResult.rounds` 之和）。
    cumulative_rounds: u32,
    /// 本**进程 run** 累计的模型轮数（跨请求；`run_end.rounds` 用）。
    run_rounds: &'a mut u32,
    out_tx: &'a mpsc::Sender<Outbound<CodingView>>,
    sink_rx: &'a mut mpsc::Receiver<Envelope>,
    boundary_rx: &'a mut mpsc::Receiver<Boundary>,
    /// subagent 派发能力（装配期冻结；`None` = 未启用）。
    subagent: Option<&'a crate::subagent::SubagentSetup>,
    /// 本请求累计的子 agent usage（回合末 drain 进 `RunResult.usage`）。
    subagent_totals: Arc<crate::subagent::UsageTotals>,
    /// debug sink（关闭时 `None`）：事件咽喉、回合边界与 Abort 记录共用。
    debug: Option<&'a mut DebugSink>,
    /// 当前会话 id（debug 记录的 `session_id`；`/new` / `/resume` 后由外层刷新）。
    debug_session_id: &'a str,
}

/// 一个回合的结局：`Ok` 是本回合的 `RunResult`，`Err` 是模型 / 工具失败
/// （`BasicLoop` 已在返回前发 `RunFailed`）。二者都属**本回合**；它不与
/// [`AppLoopError`]（「UI 侧已收摊」）混为一谈 —— 后者才终止整个 app 循环。
type TurnOutcome = Result<RunResult, LoopError>;

/// 从事件流里累计**本回合已开始的轮数**（`RoundStarted` 每轮恰好一条）。
///
/// # 为什么不能只看 `RunResult.rounds`
///
/// `RunResult` 只在**成功**路径上被构造；模型调用失败时 `BasicLoop` 直接
/// `return Err`，那份 `RunResult` 随栈丢弃 ⇒ 只看它会把失败回合一律记成 0 轮，
/// 日志里的 `rounds=` 与实际跑过的轮数不符。事件流是成功与失败**两条路径共有**的
/// 唯一来源（`RunFailed` 不带 rounds，`RunFinished` 只在成功时出现）。
fn count_round(env: &Envelope, observed: &mut u32) {
    if matches!(env.event, AgentEvent::RoundStarted { .. }) {
        *observed += 1;
    }
}

/// 本回合用于日志与 `cumulative_rounds` 的轮数：**成功取 `RunResult`，失败取事件流累计**。
///
/// 抽成纯函数只为可测 —— 内联在 `run_turn_once` 里时，「失败取 `observed`」这一步
/// 没有任何用例够得着（日志去向是真实 HOME 下的文件，测试无法注入）。
fn turn_rounds(outcome: &TurnOutcome, observed: u32) -> u32 {
    match outcome {
        Ok(result) => result.rounds,
        Err(_) => observed,
    }
}

/// 退出前收尾 debug sink：排空队列并写 `run_end`（best-effort，最多 2 s）。
///
/// `exit_code` 区分正常退出与「UI 已收摊」；两者的事件名都可能是 `RunFinished`
/// （TUI 没有 `RunFailed` 终局事件）。
fn flush_debug(debug: &mut Option<DebugSink>, stats: &TurnStats, rounds: u32, exit_code: i32) {
    if let Some(sink) = debug.as_mut() {
        let usage = stats.cumulative_usage();
        sink.flush_run(crate::debug_log::RunEndMeta {
            stop_reason: if exit_code == 0 {
                "RunFinished"
            } else {
                "RunFailed"
            },
            rounds,
            exit_code,
            usage: Some(&usage),
        });
    }
}

/// 出站失败（UI 已收摊）时先收尾 debug，再向上传播 —— 与 [`flush_debug`] 同一出口。
async fn send_or_flush(
    tx: &mpsc::Sender<Outbound<CodingView>>,
    out: Outbound<CodingView>,
    debug: &mut Option<DebugSink>,
    stats: &TurnStats,
    rounds: u32,
) -> Result<(), AppLoopError> {
    match send(tx, out).await {
        Ok(()) => Ok(()),
        Err(e) => {
            flush_debug(debug, stats, rounds, 1);
            Err(e)
        }
    }
}

/// 逐轮计时状态机 —— 定义在 [`crate::round_clock`]（两条入口共用同一套口径）。
use crate::round_clock::{RoundClock, turn_cache, turn_line};

/// 跑**一个回合**：排空残留边界 → `begin_turn` → 新建边界源 → `run_turn` 与事件
/// 转发交错 → 回合末排空事件信道。**记账 / 写穿 meta 不在此处** —— 由调用方拿到
/// [`TurnOutcome`] 后完成（复核判据也要用这份结果）。
///
/// **函数第一步就是排空边界信道**：上一回合结束后才到的 `Abort` 在语义上已是废的
/// （UI 的 `is_turning` 复位最长滞后一个 poll 周期），**上一回合的取消不得取消本回合**。
///
/// **`Abort` 永久置位**（模块注释 ③）：下面的 `QueueBoundarySource::new()` 只防标志位
/// 复用，防不了**队列里那条已投递的边界消息** —— 故排空必须在新建边界源**之前**。
///
/// **返回 `Result<_, AppLoopError>`**：外层只对「UI 侧已收摊」（`send` 失败）
/// 用 `?` 终止循环；`run_turn` 的失误留在 [`TurnOutcome`] 内交给调用方落日志。
async fn run_turn_once(
    ctx: &mut TurnCtx<'_>,
    input: AgentInput,
) -> Result<TurnOutcome, AppLoopError> {
    // 排空上一回合残留的边界消息（含 `Abort`）—— 契约见函数头。
    while ctx.boundary_rx.try_recv().is_ok() {}

    // 回合号从 1 起（`begin_turn` 把它带进 `Envelope.turn`）。
    *ctx.turns += 1;
    // `turn_start` 必须在回合开跑前取输入规模（`input` 随后被移动进 `run_turn`）。
    if let Some(sink) = ctx.debug.as_deref_mut() {
        crate::record_turn_start(
            sink,
            ctx.debug_session_id,
            *ctx.turns,
            crate::message_bytes(&input.message),
            ctx.config.max_turn_duration(),
        );
    }
    // **每回合刷新 context window**（唯一调用点，结构上不可能漏）。
    //
    // `/login`、`/model` 会在运行中换 provider/model，窗口差可达 16 倍
    // （128k ↔ 1M）：只在 `build_agent` 解析的话，用户 `/login` 到 1M
    // 模型后仍按旧窗口压缩 —— 这正是「1M 模型在 11 万 token 就被压缩」
    // 的后半截根因。
    //
    // 放在回合**开头**（而非 login/set_model 各加一处）：命令的实现散在
    // `capabilities`，而窗口由 `config` 派生 —— 一处读取、不可能漂移。
    ctx.agent.set_context_window(ctx.config.context_window());
    // 本回合**实际跑过的轮数**，从事件流累计（见 [`count_round`] 的理由）——
    // 成功与失败两条路径都从这里取，不依赖 `RunResult` 是否存在。
    let mut observed_rounds: u32 = 0;
    // 逐轮读数（见 [`RoundClock`]）—— 与轮数累计共用同一个事件咽喉。
    // 摘要档位只影响 debug 记录；关闭时取 `Default` 的同一取值（`Head`）。
    let args_mode = ctx
        .debug
        .as_ref()
        .map(|sink| sink.args_mode())
        .unwrap_or(ArgsMode::Head);
    let mut clock = RoundClock::new(args_mode);
    let outcome = {
        let ports = ctx.wiring.ports();
        ports.events.begin_turn(*ctx.turns);
        // 本回合专属的边界源：`Abort` 的标志位随回合作废。
        let boundary = QueueBoundarySource::new();
        // subagent：每回合造一份 runner（派发计数器天然 per-turn；每次派发建新子 Agent）。
        let runner = ctx
            .subagent
            .map(|s| s.make_runner(Some(ctx.config), ports.model, ctx.subagent_totals.clone()));
        let mut agent_ports =
            AgentPorts::new(ports.model, ports.session, ports.events, Some(&boundary));
        if let Some(r) = &runner {
            agent_ports = agent_ports.with_subagent(r);
        }
        let turn = ctx.agent.run_turn(input, agent_ports);
        tokio::pin!(turn);
        // 预算计时器**只允许触发一次**：到点后置位，`select!` 的 `if` 前置条件随即
        // 禁用该分支（否则 `sleep_until` 会持续 Ready ⇒ 忙等 + 重复发说明行）。
        let mut budget_exhausted = false;
        loop {
            tokio::select! {
                result = &mut turn => break result,
                // 事件转发。**必须用 `Some(..) = …` 模式形式**：
                // 信道关闭后 `recv()` 会立即返回 `None`，裸分支
                // （`env = … => match`）会让 select 每次都立刻选中它
                // → 忙等。模式不匹配时 tokio 会**禁用**该分支。
                Some(env) = ctx.sink_rx.recv() => {
                    count_round(&env, &mut observed_rounds);
                    crate::observe_event(
                        ctx.debug.as_deref_mut(),
                        &mut clock,
                        &env,
                        ctx.debug_session_id,
                    );
                    send(ctx.out_tx, Outbound::Event(env)).await?;
                }
                // 轮边界中途插话 / 中止：UI 写信道 ②，这里泵进
                // `BoundarySource`，由 `BasicLoop` 在轮边界拉取。
                Some(b) = ctx.boundary_rx.recv() => {
                    if matches!(&b, Boundary::Abort)
                        && let Some(sink) = ctx.debug.as_deref_mut()
                    {
                        crate::record_boundary(
                            sink,
                            ctx.debug_session_id,
                            Some(*ctx.turns),
                            "enqueued",
                            "abort",
                            Some("ui"),
                        );
                    }
                    boundary.push(b)
                }
                // wall-clock 预算到点：**先**发一条说明行，**再**向本回合的边界源
                // push `Abort`。该回合以 `Cancelled` 收场，请求到此结束。
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(ctx.deadline)),
                    if !budget_exhausted => {
                    budget_exhausted = true;
                    let secs = ctx.config.max_turn_duration().as_secs();
                    send(ctx.out_tx, Outbound::Output(format!(
                        "budget exhausted: wall-clock budget of {secs}s elapsed — aborting this turn"
                    ))).await?;
                    boundary.push(Boundary::Abort);
                    if let Some(sink) = ctx.debug.as_deref_mut() {
                        crate::record_boundary(
                            sink,
                            ctx.debug_session_id,
                            Some(*ctx.turns),
                            "enqueued",
                            "abort",
                            Some("budget"),
                        );
                    }
                }
            }
        }
    };
    // 子 agent 的 usage **上卷**：预算独立、记账上卷（ADR-0029 决策）。
    // 追加进 `RunResult.usage` ⇒ 下游（`TurnStats` / meta / 退出码）看到真实总消耗。
    let outcome = match outcome {
        Ok(mut r) => {
            r.usage = r.usage + ctx.subagent_totals.drain();
            Ok(r)
        }
        Err(e) => Err(e),
    };
    // **回合结束后必须排空事件信道**：`select!` 分支的选择是随机的，
    // 它完全可能先选中 `turn` 分支 —— 此刻 `RunFinished` 等收尾事件
    // 还躺在信道里。不排空就会晚一整回合才转发（UI 的 `is_turning`
    // 不复位、turn 摘要不显示），且会插到本条 `View` 之后。
    // 回合已结束 → 生产者不会再发，`try_recv` 排到 `Empty` 即止。
    while let Ok(env) = ctx.sink_rx.try_recv() {
        count_round(&env, &mut observed_rounds);
        crate::observe_event(
            ctx.debug.as_deref_mut(),
            &mut clock,
            &env,
            ctx.debug_session_id,
        );
        send(ctx.out_tx, Outbound::Event(env)).await?;
    }
    // 兜底：`RoundStarted` 已开轮、终局事件却没跟到（失败路径理论上会发生）时，
    // 仍为那一轮落一行 —— 排障时「跑了但没读数」比「读数为 0」更难查。
    for line in clock.take_lines() {
        logging::log(&line);
    }

    // §4.9 可观测性：每个回合边界经**日志**落一行（对 TUI 不可见，供排障）。
    // `elapsed` / `budget_remaining` 都取**整数秒**便于 grep；`budget_remaining`
    // 走 `saturating_duration_since` ⇒ 到点后为 0，绝不出现负数或 panic。
    // 失败回合拿不到 `RunResult` ⇒ 用事件流里数到的轮数（见 [`turn_rounds`]）。
    let rounds = turn_rounds(&outcome, observed_rounds);
    *ctx.run_rounds = ctx.run_rounds.saturating_add(rounds);
    ctx.cumulative_rounds = ctx.cumulative_rounds.saturating_add(rounds);
    let remaining = ctx.deadline.saturating_duration_since(Instant::now());
    logging::log(&turn_line(
        *ctx.turns,
        rounds,
        ctx.request_started.elapsed(),
        ctx.cumulative_rounds,
        remaining,
        turn_cache(&outcome),
    ));

    // debug 侧的回合收尾（只进文件，绝不影响上面的文本行）。
    // 以 `Cancelled` 收场 ⇒ 补一条 `observed`：`enqueued` 说明 Abort 已投递，
    // `observed` 说明回合确实看到了它（两者都缺一不可）。
    if let Some(sink) = ctx.debug.as_deref_mut() {
        if matches!(&outcome, Ok(r) if r.stop_reason == StopReason::Cancelled) {
            crate::record_boundary(
                sink,
                ctx.debug_session_id,
                Some(*ctx.turns),
                "observed",
                "abort",
                None,
            );
        }
        crate::record_turn_end(
            sink,
            ctx.debug_session_id,
            *ctx.turns,
            rounds,
            ctx.request_started.elapsed(),
            remaining,
            turn_cache(&outcome),
        );
    }

    Ok(outcome)
}

/// 展开 `/skill-name <args>`：命中已知 skill 时把消息**重写**为带正文的信封
/// （`<skill name="…" location="…">正文</skill>`），否则**原样返回**。
///
/// # 为什么是「重写整条消息」而不是追加
///
/// 设计 §选择理由 B：TUI **不读盘**、也不持 frontmatter 逻辑，它只「认名」并发一条
/// 原文的 `Request::Prompt("/name args")`；正文由 app 侧在这里读盘注入。形态与
/// Claude Code 注入的 user 消息一致（设计 §CC 兼容性 采纳表）。
///
/// # 三个「不」
///
/// - 首块**不是 `Text`**（例如带图/多块消息）⇒ 不展开（只认纯文本首块的 `/` 命名空间）。
/// - 未知命令 / `user-invocable: false` / 正文读不出来 ⇒ `expand_invocation` 返回
///   `None` ⇒ 原样返回（宁可发一条普通消息，也不塞半截指令）。
/// - **不缓存**正文：skill 目录可写（agent 会往里写文件），缓存会给出陈旧指令。
fn expand_skill_invocation(message: Message, skills: &[crate::skills::SkillEntry]) -> Message {
    let Some(text) = first_text(&message) else {
        return message;
    };
    match crate::skills::expand_invocation(&text, skills) {
        Some(expanded) => {
            logging::log(&format!(
                "skill invocation expanded ({} bytes) for this turn",
                expanded.len()
            ));
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: expanded }],
            }
        }
        None => message,
    }
}

/// 首块是 `Text` 则取其文本（owned，避免与「移动 `message`」冲突）。
fn first_text(message: &Message) -> Option<String> {
    match message.content.first() {
        Some(ContentBlock::Text { text }) => Some(text.clone()),
        _ => None,
    }
}

/// 发送一条出站消息；UI 侧已收摊（信道关闭）时返回 `Err` 让循环退出。
async fn send(
    tx: &mpsc::Sender<Outbound<CodingView>>,
    out: Outbound<CodingView>,
) -> Result<(), AppLoopError> {
    tx.send(out)
        .await
        .map_err(|_| AppLoopError::from("outbound channel closed (UI exited)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use ys_core::{ContentBlock, Message, Role, StopReason, ToolCallId, Usage};
    use ys_event::AgentEvent;
    use ys_model::{
        FinishReason, MockModel, Model, ModelError, ModelEvent, ModelEventSink, ModelRequest,
        ModelResponse,
    };

    use crate::channel::ChannelSink;
    use ys_tools_web::{FetchedDir, WebConfig};

    /// 所有跨任务 `await` 都套这个超时：死锁回归必须表现为**失败**，而不是把测试挂住。
    /// 该值只是死锁守卫，真死锁仍会失败；取 15s 是为了避免并行测试负载下的误报。
    const TIMEOUT: Duration = Duration::from_secs(15);

    /// 等「第 1 次 model 调用开始」的上限。**只影响失败有多快，不影响正确性**：
    /// 超时后照样放行门控（见 `stale_abort_...` 测试）。取一个远大于「任务已被
    /// 唤醒 → 跑到首次 model 调用」所需时间的值，正常路径下不可能撞上。
    const ENTRY_WAIT: Duration = Duration::from_millis(500);

    // ------------------------------------------------------------------ 测试台

    /// 临时目录（`Drop` 自动清理）。
    ///
    /// 名字含**进程内原子序号 + `process::id()`**（并行安全，血的教训）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "yushan_app_loop_{tag}_{}_{seq}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// app 循环 + 三条信道的对端。
    ///
    /// 故意**不实现 `Drop`**：测试 7 要把各字段移出去（`drop(request_tx)` 模拟 UI
    /// 收摊），而有 `Drop` 的类型不允许字段被部分移出。任务随 runtime 一起回收。
    struct Harness {
        requests: mpsc::Sender<Request>,
        boundaries: mpsc::Sender<Boundary>,
        /// 后台模型目录信道（`main` 的角色）：投一条缓存 = 模拟拉取完成。
        models: mpsc::Sender<crate::provider::ModelCache>,
        outbound: mpsc::Receiver<Outbound<CodingView>>,
        handle: tokio::task::JoinHandle<Result<(), AppLoopError>>,
        /// 与 app 循环共享的会话路径快照（退出提示行的取数口）。
        session_path: SessionPathHandle,
        /// 与 app 循环共享的工作区绑定快照（退出行的取数口）。
        worktree: WorktreeHandle,
        tmp: TempDir,
    }

    impl Harness {
        fn temp_path(&self, name: &str) -> PathBuf {
            self.tmp.join(name)
        }
    }

    /// 起 app 循环。
    ///
    /// - `persistent`：`true` = 会话落盘到临时 `sessions/`（`/new` 换文件可见）。
    /// - `prefilled`：在**循环开跑之前**就躺进边界信道的消息（复现「上一回合残留」）。
    ///
    /// 测试绝不碰真实的 `~/.yushan`：auth / state / 会话文件全部改到临时目录。
    async fn spawn_app_loop(
        model: Box<dyn Model>,
        persistent: bool,
        prefilled: Vec<Boundary>,
    ) -> Harness {
        spawn_app_loop_with(model, persistent, prefilled, |_, _| {}).await
    }

    /// 同上，但允许在起循环前改写 `Config`（第二个参数是临时目录，可往里写
    /// `config.json`）—— 供「窗口每回合刷新」用：把某个 model 的 context window
    /// 压小，自动压缩这条**唯一可观测**的路径才会被触发。
    async fn spawn_app_loop_with(
        model: Box<dyn Model>,
        persistent: bool,
        prefilled: Vec<Boundary>,
        tweak: impl FnOnce(&mut Config, &PathBuf),
    ) -> Harness {
        spawn_app_loop_full(
            model,
            persistent,
            prefilled,
            tweak,
            |b| b,
            |_| None,
            None,
            false,
            false,
        )
        .await
    }

    /// 同上，但打开 debug sink（调用方**整段持有** `env_lock`，并把
    /// `YUSHAN_LOG_DIR` 指向临时目录）。
    async fn spawn_app_loop_debug(
        model: Box<dyn Model>,
        persistent: bool,
        prefilled: Vec<Boundary>,
    ) -> Harness {
        spawn_app_loop_full(
            model,
            persistent,
            prefilled,
            |_, _| {},
            |b| b,
            |_| None,
            None,
            true,
            true,
        )
        .await
    }

    /// 最底层起法：除 `Config` 外还允许改写 [`ys_runtime::AgentBuilder`]
    /// （例如把 [`ys_component::RunLimits::max_rounds`] 调到 app 产品默认的
    /// `u32::MAX`）—— 生产侧由 `resolve_limits` 装配的限制，测试在此显式重建。
    ///
    /// 最后一个参数是**启动期播种的工作区绑定**（`None` = 共享工作区）；换工作区的
    /// 用例在此传一棵树，其余用例传 `None`。
    #[allow(clippy::too_many_arguments)]
    async fn spawn_app_loop_full(
        model: Box<dyn Model>,
        persistent: bool,
        prefilled: Vec<Boundary>,
        tweak: impl FnOnce(&mut Config, &PathBuf),
        build_agent: impl FnOnce(ys_runtime::AgentBuilder) -> ys_runtime::AgentBuilder,
        subagent: impl FnOnce(&Path) -> Option<crate::subagent::SubagentSetup>,
        worktree: Option<WorktreeBinding>,
        debug_flag: bool,
        // 调用方**已持有** `env_lock`（debug 测试整段持有，避免并行测试改环境）。
        // 为 `true` 时不再自行加锁 —— 同线程重复加锁会死锁。
        env_prelocked: bool,
    ) -> Harness {
        let tmp = TempDir::new("harness");
        let subagent = subagent(&tmp.0);
        let (sink, sink_rx) =
            ChannelSink::new(64, ys_protocol::LifecyclePolicy::StopWhenConsumerGone);
        let agent = build_agent(ys_runtime::AgentBuilder::new())
            .build()
            .unwrap();

        // `Config::from_env` 会 `getenv` 凭证 key，而 `config.rs` 的用例正在
        // `env_lock` 下 `set_var` / `remove_var` —— 只读也须串行。
        // 锁**只圈住这次 `getenv`**，不跨下方的 `.await`：`MutexGuard` 非 `Send`，
        // 跨 await 会让整个 future 失去 `Send`（且本就无故长持全局锁）。
        let mut config = if env_prelocked {
            Config::from_env().unwrap()
        } else {
            let _guard = crate::test_env::env_lock();
            Config::from_env().unwrap()
        };
        config.registry.set_auth_override(tmp.join("auth.json"));
        config.api_base = None;
        config.api_key = None;
        config.provider = None;
        // 与生产侧同规：绑定到树时启动 cwd 就是树根（`worktree attach` 改写 `config.cwd`）；
        // 否则是共享工作区（`Config::from_env` 取 `current_dir()`，此处以临时目录顶替）。
        config.cwd = worktree
            .as_ref()
            .map(|b| b.path.clone())
            .unwrap_or_else(|| tmp.0.clone());
        tweak(&mut config, &tmp.0);

        let mut state_store = StateStore::new();
        state_store.set_override(tmp.join("state.json"));

        let wiring = if persistent {
            Wiring::persistent_new(Some(model), Box::new(sink), tmp.join("sessions"))
                .await
                .unwrap()
        } else {
            Wiring::ephemeral(Some(model), Box::new(sink))
        };

        // 与生产侧同规：debug sink 在 `Wiring` 建好、会话路径已知之后打开，
        // 并由「启动期一方」写第一条 `run_start`。关闭时 `open_enabled` 早退。
        let mut debug = crate::debug_log::open_enabled(debug_flag, wiring.session_path());
        if let Some(sink) = debug.as_mut() {
            let session_id = crate::session_id_for(wiring.session_path());
            crate::emit_run_start(sink, &config, wiring.session_path(), &session_id, &[]);
        }

        let (requests, request_rx) = mpsc::channel(8);
        let (boundaries, boundary_rx) = mpsc::channel(8);
        let (out_tx, outbound) = mpsc::channel(64);
        // 后台模型目录信道：harness 自带一个发送端（生产侧是 `main` 的后台任务）。
        // 测试不投时该分支恒挂起，行为与「无后台拉取」一致。
        let (models, models_rx) = mpsc::channel(4);

        // 与生产侧同规：启动态由建 `Wiring` 的一方播种（`/new` 之后由循环刷新）。
        let session_path =
            SessionPathHandle::new(wiring.session_path().map(std::path::Path::to_path_buf));
        let app_session_path = session_path.clone();

        // 与生产侧同规：启动期由 `main` 播种工作区绑定；`/resume` 后由循环刷新。
        let worktree = WorktreeHandle::new(worktree);
        let app_worktree = worktree.clone();

        // 装配工厂（换工作区时重建 agent 用）：确定性 offline web、空 skill 目录、
        // 临时产物目录 —— 绝不碰真实 `~/.yushan`。
        let factory = crate::assembly::AgentFactory::new(
            WebConfig::offline(FetchedDir::Temp(tmp.join("fetched"))),
            crate::skills::catalogue(Vec::new(), crate::skills::Budget::default()),
            ys_component::RunLimits::default(),
            Duration::from_secs(30),
            Arc::new(crate::subagent::SubagentsDir::at(
                tmp.join("subagents"),
                false,
            )),
        );
        // 共享工作区 = harness 临时目录（生产侧是进程启动目录）。
        let default_workspace = tmp.0.clone();

        for b in prefilled {
            boundaries.try_send(b).unwrap();
        }

        let handle = tokio::spawn(run(
            agent,
            wiring,
            config,
            state_store,
            TurnStats::default(),
            Instant::now(),
            request_rx,
            boundary_rx,
            out_tx,
            sink_rx,
            Some(models_rx),
            subagent,
            factory,
            default_workspace,
            app_session_path,
            app_worktree,
            debug,
            None,
            Vec::new(),
        ));

        Harness {
            requests,
            boundaries,
            models,
            outbound,
            handle,
            session_path,
            worktree,
            tmp,
        }
    }

    /// 便捷：一次性会话 + 单个 [`MockModel`]。
    async fn spawn_with_mock(model: MockModel) -> Harness {
        spawn_app_loop(Box::new(model), false, Vec::new()).await
    }

    fn user_message(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    fn prompt(text: &str) -> Request {
        Request::Prompt(user_message(text))
    }

    /// 收一条出站消息（带超时 —— 挂起即失败）。
    async fn recv_out(rx: &mut mpsc::Receiver<Outbound<CodingView>>) -> Outbound<CodingView> {
        tokio::time::timeout(TIMEOUT, rx.recv())
            .await
            .expect("收取 Outbound 超时（疑似死锁）")
            .expect("app 循环不应提前收摊")
    }

    /// 收出站消息直到一条 `View`（每个请求处理完必刷一次）。
    async fn collect_until_view(h: &mut Harness) -> Vec<Outbound<CodingView>> {
        let mut msgs = Vec::new();
        loop {
            let m = recv_out(&mut h.outbound).await;
            let is_view = matches!(m, Outbound::View(_));
            msgs.push(m);
            if is_view {
                return msgs;
            }
        }
    }

    /// 取出末尾的 `View`（每个请求处理完必有），剩下的留给顺序断言。
    fn take_view(msgs: &mut Vec<Outbound<CodingView>>) -> CodingView {
        match msgs.pop() {
            Some(Outbound::View(view)) => view,
            other => panic!("最后一条出站消息应为 View，得到 {other:?}"),
        }
    }

    fn events_of(msgs: &[Outbound<CodingView>]) -> Vec<&Envelope> {
        msgs.iter()
            .filter_map(|m| match m {
                Outbound::Event(env) => Some(env),
                _ => None,
            })
            .collect()
    }

    /// 失败回合的轮数取「事件流累计」—— 这条修复的全部意义。
    ///
    /// 背景（真实会话 `1791990139`）：一个回合跑了 26 轮、342 秒后模型流中断，
    /// 日志记的却是 `rounds=0`，因为 `RunResult` 只在成功路径上被构造。
    ///
    /// 变异：把 `turn_rounds` 的 `Err` 分支改回 `0` ⇒ 第二条断言变红；
    /// 把 `Ok` 分支改成也用 `observed` ⇒ 第一条断言变红。
    #[test]
    fn turn_rounds_prefers_result_then_observed() {
        let ok: TurnOutcome = Ok(ys_loop::RunResult {
            stop_reason: StopReason::Completed,
            usage: Usage::default(),
            rounds: 4,
            final_message: None,
        });
        assert_eq!(turn_rounds(&ok, 9), 4, "成功时以 RunResult 为准");

        let failed: TurnOutcome = Err(ys_loop::LoopError::Tool("boom".into()));
        assert_eq!(turn_rounds(&failed, 26), 26, "失败时用事件流累计的轮数");
        assert_eq!(
            turn_rounds(&failed, 0),
            0,
            "首轮即失败 ⇒ 事件流一条都没收到，0 才是对的"
        );
    }

    /// 事件的**种类序列** —— 锁定顺序，而不只是集合。
    fn event_kinds(msgs: &[Outbound<CodingView>]) -> Vec<&'static str> {
        events_of(msgs)
            .iter()
            .map(|e| match &e.event {
                AgentEvent::UserMessage { .. } => "UserMessage",
                AgentEvent::RoundStarted { .. } => "RoundStarted",
                AgentEvent::ModelTextDelta { .. } => "ModelTextDelta",
                // Step 7 的逐轮 usage 上报（每轮一条，在终局事件之前）。
                AgentEvent::RoundUsage { .. } => "RoundUsage",
                AgentEvent::RunFinished { .. } => "RunFinished",
                AgentEvent::RunFailed { .. } => "RunFailed",
                _ => "Other",
            })
            .collect()
    }

    fn stop_reasons(events: &[&Envelope]) -> Vec<StopReason> {
        events
            .iter()
            .filter_map(|e| match &e.event {
                AgentEvent::RunFinished { stop_reason, .. } => Some(stop_reason.clone()),
                _ => None,
            })
            .collect()
    }

    // ------------------------------------- probe 型 model（供「回合进行中」注入边界）

    /// probe model 的行为计划。
    #[derive(Clone, Copy)]
    enum ProbePlan {
        /// 直到请求里出现含 `marker` 的文本消息才收尾；之前每轮回一个**不存在**的
        /// tool call，把回合推过一轮又一轮的轮边界。
        UntilMarker(&'static str),
        /// 恒回不存在的 tool call：`Abort` 场景下，下一轮的 abort 探针先于 model 调用命中。
        AlwaysToolCall,
        /// 第 1 次（被 gate 卡住的那次）回不存在的 tool call，第 2 次回结束文本。
        ///
        /// 供「残留 Abort 是否被丢弃」的测试用：**必须**让回合跨过一个轮边界，
        /// 泄漏的 `Abort` 才有机会被 `BasicLoop` 的探针看见（否则第 1 轮直接
        /// 正常收尾，泄漏与不泄漏都产出 `Completed`，测试就分不出红绿）。
        ToolCallThenFinal,
        /// 第 `calls` 列出的每次（以及其后的每次）调用回结束文本，其余调用回
        /// **不存在**的 tool call —— 把回合推过若干轮。
        CompleteAtCalls(&'static [u32]),
    }

    struct ProbeInner {
        plan: ProbePlan,
        /// 第 1 次 `complete` 阻塞在此 —— 测试得以在「回合进行中」投递 `Boundary`。
        gate: tokio::sync::Notify,
        entered: mpsc::UnboundedSender<u32>,
        requests: Mutex<Vec<ModelRequest>>,
    }

    struct ProbeModel {
        inner: Arc<ProbeInner>,
    }

    struct ProbeHandle {
        entered: mpsc::UnboundedReceiver<u32>,
        inner: Arc<ProbeInner>,
    }

    fn probe_model(plan: ProbePlan) -> (ProbeModel, ProbeHandle) {
        let (entered_tx, entered) = mpsc::unbounded_channel();
        let inner = Arc::new(ProbeInner {
            plan,
            gate: tokio::sync::Notify::new(),
            entered: entered_tx,
            requests: Mutex::new(Vec::new()),
        });
        (
            ProbeModel {
                inner: Arc::clone(&inner),
            },
            ProbeHandle { entered, inner },
        )
    }

    impl ProbeHandle {
        /// 等到第 `n` 次 model 调用**已经开始**（此刻回合正卡在本轮 model 调用里）。
        async fn wait_entered(&mut self, n: u32) {
            loop {
                let call = tokio::time::timeout(TIMEOUT, self.entered.recv())
                    .await
                    .expect("model 未被调用：超时")
                    .expect("probe 通道不应关闭");
                if call == n {
                    return;
                }
            }
        }

        /// 放行第 1 次 `complete`。`notify_one` 会留存一个 permit，
        /// 「先放行、后 await」也不会丢通知。
        fn release(&self) {
            self.inner.gate.notify_one();
        }

        /// 让调度器把 app 任务跑一会儿 —— 确保刚投进边界信道的消息已被泵进本轮的
        /// `BoundarySource`（此刻 model 仍阻塞在 gate 上，`select!` 必走边界分支）。
        ///
        /// 默认 `#[tokio::test]` 是 current-thread runtime：`yield_now` 会让出给
        /// app 任务，故这一步是**确定性**的，不是定时赌博。
        async fn let_app_task_pump(&self) {
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
        }

        fn requests(&self) -> Vec<ModelRequest> {
            self.inner.requests.lock().unwrap().clone()
        }
    }

    fn has_text(request: &ModelRequest, needle: &str) -> bool {
        request.messages.iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text.contains(needle)))
        })
    }

    fn text_response(text: &str) -> ModelResponse {
        ModelResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: text.into() }],
            },
            usage: Usage::default(),
            finish_reason: FinishReason::Stop,
        }
    }

    #[async_trait::async_trait]
    impl Model for ProbeModel {
        fn model_id(&self) -> &str {
            "probe"
        }

        async fn complete(
            &self,
            request: ModelRequest,
            sink: &mut dyn ModelEventSink,
        ) -> Result<ModelResponse, ModelError> {
            let call = {
                let mut reqs = self.inner.requests.lock().unwrap();
                reqs.push(request.clone());
                reqs.len() as u32
            };
            let _ = self.inner.entered.send(call);
            if call == 1 {
                self.inner.gate.notified().await;
            }

            // 截断计划单独走一条：收尾响应带 `FinishReason::Length`（⇒ `Truncated`）。
            let done = match self.inner.plan {
                ProbePlan::UntilMarker(marker) => has_text(&request, marker),
                ProbePlan::AlwaysToolCall => false,
                ProbePlan::ToolCallThenFinal => call > 1,
                // 列出的调用收尾；其后也收尾（阈值可给多段）。
                ProbePlan::CompleteAtCalls(calls) => {
                    calls.contains(&call) || calls.last().is_some_and(|&last| call > last)
                }
            };

            if done {
                sink.emit(ModelEvent::TextDelta {
                    text: "done".into(),
                })?;
                Ok(text_response("done"))
            } else {
                Ok(ModelResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::ToolUse {
                            id: ToolCallId("probe-1".into()),
                            name: "no_such_tool".into(),
                            arguments: serde_json::Value::Null,
                        }],
                    },
                    usage: Usage::default(),
                    finish_reason: FinishReason::Stop,
                })
            }
        }
    }

    // -------------------------------------------------------------------- 测试

    /// `cancel_on_abort`：future 先完成 ⇒ `Some(结果)`，与不套本函数逐字等价
    /// （命令正常收场不能被取消口影响）。
    #[tokio::test]
    async fn cancel_on_abort_passes_through_completed_future() {
        let (_tx, mut rx) = mpsc::channel::<Boundary>(4);
        assert_eq!(cancel_on_abort(async { 7 }, &mut rx).await, Some(7));
    }

    /// `Abort` 先到 ⇒ `None`：**永不完成的 future 也必须立刻返回**。
    ///
    /// 这正是 `/login` 挂住的场景：`fetch_models` 的请求没回来时，Esc 是唯一出口。
    /// 变异：把 `Some(Boundary::Abort) => return None` 改成 `continue` ⇒ 本用例挂死。
    #[tokio::test]
    async fn cancel_on_abort_returns_none_when_abort_wins() {
        let (tx, mut rx) = mpsc::channel::<Boundary>(4);
        tx.send(Boundary::Abort).await.unwrap();
        let out = cancel_on_abort(std::future::pending::<u32>(), &mut rx).await;
        assert_eq!(out, None, "Abort 必须结束等待");
    }

    /// 取消 = **真的丢掉**那条 future（reqwest 的请求随之取消），
    /// 不是丢到后台继续跑、把凭证改一半。
    ///
    /// 变异：`Abort` 臂不返回（继续持有 `fut`）⇒ 本用例挂死；
    /// 若改用 `tokio::spawn` 则 guard 不会在返回前 drop。
    #[tokio::test]
    async fn cancel_on_abort_drops_the_future() {
        use std::sync::atomic::AtomicBool;

        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let flag = Arc::new(AtomicBool::new(false));
        let guard = DropFlag(Arc::clone(&flag));
        let (tx, mut rx) = mpsc::channel::<Boundary>(4);
        tx.send(Boundary::Abort).await.unwrap();

        let out = cancel_on_abort(
            async move {
                let _hold = &guard;
                std::future::pending::<u32>().await
            },
            &mut rx,
        )
        .await;

        assert_eq!(out, None);
        assert!(flag.load(Ordering::SeqCst), "future 必须被 drop");
    }

    /// 命令期间收到的 `Steer` 与命令无关：丢掉它并**继续等**。
    /// 下一回合的起点本来就会排空残留边界消息（`run_turn_once` 头部），
    /// 故丢弃不改变任何已有语义。
    ///
    /// 变异：把 `Some(_) => continue` 改成返回 ⇒ 本用例变红。
    #[tokio::test]
    async fn cancel_on_abort_ignores_steer() {
        let (tx, mut rx) = mpsc::channel::<Boundary>(4);
        tx.send(Boundary::Steer(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "插一句".into(),
            }],
        }))
        .await
        .unwrap();

        let out = cancel_on_abort(
            async {
                tokio::task::yield_now().await;
                7
            },
            &mut rx,
        )
        .await;
        assert_eq!(out, Some(7), "Steer 不算取消");
    }

    /// 1. **单一出站写者 / 顺序确定性**（设计 §8）：事件按到达顺序、终局 `View`
    ///    落在最后、`Output` 绝不插到事件中间、`Envelope.turn` 随回合单调。
    ///
    ///    变异：让事件绕过 app 转发（在 `select!` 分支里丢弃 `env`）→ 本测试的
    ///    `event_kinds` 断言变红。
    #[tokio::test]
    async fn outbound_sequence_is_ordered_with_view_last() {
        let model = MockModel::new("mock");
        model.push_text("hello");
        model.push_text("again");
        let mut h = spawn_with_mock(model).await;

        // 回合 1：事件（种类顺序固定）→ View。
        h.requests.send(prompt("one")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        assert_eq!(view.turn_count, 1);
        assert!(
            msgs.iter().all(|m| matches!(m, Outbound::Event(_))),
            "回合期间只应有事件（唯一的 View 在末尾）：{msgs:?}"
        );
        assert_eq!(
            event_kinds(&msgs),
            vec![
                "UserMessage",
                "RoundStarted",
                "ModelTextDelta",
                // 每条模型轮新增一条 `RoundUsage`（Step 7），位于终局事件之前。
                "RoundUsage",
                "RunFinished"
            ],
            "事件顺序必须与循环产出顺序一致：{msgs:?}"
        );
        assert!(events_of(&msgs).iter().all(|e| e.turn == 1));

        // 回合 2：新事件全部排在**上一个 View 之后**（顺序整体单调，无交叉）。
        h.requests.send(prompt("two")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        assert_eq!(view.turn_count, 2);
        assert!(
            events_of(&msgs).iter().all(|e| e.turn == 2),
            "第二个回合的信封 turn 应为 2：{msgs:?}"
        );

        // 命令：出站序列恰为 `[Output]` + 末尾 `View` —— 事件不得插队。
        h.requests.send(Request::NewSession).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        assert_eq!(view.message_count, 0, "换会话后消息数归零");
        match msgs.as_slice() {
            [Outbound::Output(text)] => {
                assert!(text.contains("New conversation started"), "{text}")
            }
            other => panic!("命令的出站序列应恰为 [Output]：{other:?}"),
        }
    }

    /// **后台模型目录**：`main` 在起屏后并发拉 `/v1/models`，目录到手经 `models`
    /// 信道送回 app 循环 —— 循环把它并入 registry 并**自发**刷一次视图，`/model`
    /// 候选随之出现这两家的条目。**全程无任何 `Request`**（不阻塞起屏的关键）。
    ///
    /// 变异：删掉循环开头 `Wake::Models` 分支里的 `apply_model_cache` + 视图刷新
    /// ⇒ 第二次 `collect_until_view` 收不到视图（挂起）→ 本用例超时变红。
    #[tokio::test]
    async fn background_model_cache_refreshes_view_without_request() {
        // 一个有凭证的 provider：候选才走 `models_for_provider`（无凭证走静态全表）。
        let model = MockModel::new("mock");
        let mut h = spawn_app_loop_with(Box::new(model), false, Vec::new(), |config, tmp| {
            config
                .registry
                .save_auth(
                    "deepseek",
                    &crate::provider::AuthEntry {
                        api_base: "https://fake.deepseek".into(),
                        api_key: "sk-x".into(),
                        model: "deepseek-chat".into(),
                    },
                )
                .unwrap();
            let _ = tmp;
        })
        .await;

        // 目录到达之前：候选里**没有**任何目录模型（模型名只有目录一个来源，
        // 不再有静态回退）。循环不在起跑时自发发视图，故用一条无输出的
        // `SetThinking` 逼一份当前视图。
        h.requests
            .send(Request::SetThinking { visible: true })
            .await
            .unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        assert!(
            !view
                .available_models
                .iter()
                .any(|c| c.model == "deepseek-v9"),
            "拉取前不应有目录模型：{:?}",
            view.available_models
        );

        // 模拟后台任务返回目录（键 = 该 provider 的 api_base）。
        let mut cache = crate::provider::ModelCache::new();
        cache.insert(
            "https://fake.deepseek".into(),
            vec![crate::provider::ApiModel {
                id: "deepseek-v9".into(),
                owned_by: None,
                name: None,
                context_window: None,
            }],
        );
        h.models.send(cache).await.unwrap();

        // 目录到达 ⇒ **无请求**也刷一次视图，候选含拉到的模型。
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        assert!(
            view.available_models
                .iter()
                .any(|c| c.model == "deepseek-v9"),
            "后台目录应进入候选：{:?}",
            view.available_models
        );
    }

    /// 2. **回合驱动**：每来一条 `Prompt` 回合号 +1，且信封的 `turn` 与之同步。
    #[tokio::test]
    async fn each_prompt_advances_turn_count() {
        let model = MockModel::new("mock");
        model.push_text("a");
        model.push_text("b");
        let mut h = spawn_with_mock(model).await;

        for expected in 1..=2u32 {
            h.requests.send(prompt("hi")).await.unwrap();
            let mut msgs = collect_until_view(&mut h).await;
            let view = take_view(&mut msgs);
            assert_eq!(view.turn_count, expected, "回合数应从 1 起递增");
            let turns: Vec<u32> = events_of(&msgs).iter().map(|e| e.turn).collect();
            assert!(!turns.is_empty(), "应至少收到一个事件");
            assert!(
                turns.iter().all(|&t| t == expected),
                "Envelope.turn 应与回合号一致：{turns:?}"
            );
        }
    }

    /// 3. `Boundary::Steer` 中途注入：该消息出现在**后续轮**的 `ModelRequest` 里
    ///    （第 1 轮请求发出时它还不在），并以 `UserMessage` 事件转发给 UI。
    #[tokio::test]
    async fn steer_is_injected_at_round_boundary() {
        const MARKER: &str = "STEER-MARKER";
        let (model, mut probe) = probe_model(ProbePlan::UntilMarker(MARKER));
        let mut h = spawn_app_loop(Box::new(model), false, Vec::new()).await;

        h.requests.send(prompt("hi")).await.unwrap();
        probe.wait_entered(1).await;
        h.boundaries
            .send(Boundary::Steer(user_message(MARKER)))
            .await
            .unwrap();
        probe.let_app_task_pump().await;
        probe.release();

        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);
        assert_eq!(
            stop_reasons(&events_of(&msgs)),
            vec![StopReason::Completed],
            "steer 之后回合应正常收尾：{msgs:?}"
        );

        let requests = probe.requests();
        assert!(
            requests.len() >= 2,
            "steer 至少应让回合多跑一轮：{}",
            requests.len()
        );
        assert!(
            !has_text(&requests[0], MARKER),
            "第 1 轮请求发出时 steer 尚未投递"
        );
        assert!(
            has_text(&requests[1], MARKER),
            "steer 必须出现在后续轮的 ModelRequest 里"
        );
        assert!(
            events_of(&msgs).iter().any(|e| matches!(
                &e.event,
                AgentEvent::UserMessage { message } if message.content.iter().any(
                    |b| matches!(b, ContentBlock::Text { text } if text.contains(MARKER)))
            )),
            "steer 也应作为 UserMessage 事件转发给 UI：{msgs:?}"
        );
    }

    /// 4. `Boundary::Abort` → 终局 `RunFinished { Cancelled }`，且 `View` 仍然发出
    ///    （UI 的 `is_turning` 才有复位依据）；下一回合不受上一回合 `Abort` 影响。
    #[tokio::test]
    async fn abort_yields_cancelled_and_view_still_emitted() {
        let (model, mut probe) = probe_model(ProbePlan::AlwaysToolCall);
        let mut h = spawn_app_loop(Box::new(model), false, Vec::new()).await;

        h.requests.send(prompt("hi")).await.unwrap();
        probe.wait_entered(1).await;
        h.boundaries.send(Boundary::Abort).await.unwrap();
        probe.let_app_task_pump().await;
        probe.release();

        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        assert_eq!(
            stop_reasons(&events_of(&msgs)),
            vec![StopReason::Cancelled],
            "中途 Abort 应以 Cancelled 收场：{msgs:?}"
        );
        assert_eq!(view.turn_count, 1, "被取消的回合仍计入回合数");

        // 回合复位：`Abort` 不得溢出到下一回合 —— 新回合的 `QueueBoundarySource`
        // 从干净状态起步，故这一回合会一直跑到轮数上限，而不是立刻被取消。
        h.requests.send(prompt("again")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);
        assert_eq!(
            stop_reasons(&events_of(&msgs)),
            vec![StopReason::MaxRounds],
            "上一回合的 Abort 不得溢出到下一回合：{msgs:?}"
        );
    }

    /// 5. `NewSession`：会话换新（`message_count == 0`）、快照刷新；旧文件保留，
    ///    新会话**惰性**（`/new` 不预建空文件，首次追加才落盘）。
    #[tokio::test]
    async fn new_session_swaps_session_and_refreshes_view() {
        let model = MockModel::new("mock");
        model.push_text("hello");
        model.push_text("again");
        let mut h = spawn_app_loop(Box::new(model), true, Vec::new()).await;

        h.requests.send(prompt("hi")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let before = take_view(&mut msgs);
        assert_eq!(before.message_count, 2, "user + assistant");
        let old_path = before.session_path.clone().expect("持久会话应有文件");
        assert!(old_path.exists(), "回合结束后会话文件应落盘");

        h.requests.send(Request::NewSession).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let after = take_view(&mut msgs);
        assert_eq!(after.message_count, 0, "换会话后消息数归零");
        let new_path = after.session_path.clone().expect("持久会话应有文件");
        assert_ne!(old_path, new_path, "应换到新会话文件");
        assert!(old_path.exists(), "旧会话文件保留");
        assert!(!new_path.exists(), "/new 惰性：未追加前不落盘空文件");
        assert_eq!(
            h.session_path.get().as_ref(),
            Some(&new_path),
            "/new 后退出提示快照应指向新会话文件"
        );

        // 追加后新文件才出现且不含旧内容。
        h.requests.send(prompt("next")).await.unwrap();
        let _ = collect_until_view(&mut h).await;
        assert!(new_path.exists(), "追加后新会话文件落盘");
        assert!(
            !std::fs::read_to_string(&new_path)
                .unwrap()
                .contains("hello"),
            "新文件不得含旧会话内容"
        );
    }

    /// 退出提示快照（[`SessionPathHandle`]）：启动态一播种就是首个会话文件；
    /// 普通回合不动它；`/new` 之后指向新文件。
    ///
    /// 变异：删掉 `Request::NewSession` 分支里的 `record` ⇒ 最后一段断言变红。
    #[tokio::test]
    async fn session_path_handle_follows_new_session() {
        let model = MockModel::new("mock");
        model.push_text("hello");
        model.push_text("again");
        let mut h = spawn_app_loop(Box::new(model), true, Vec::new()).await;

        let first = h.session_path.get().expect("持久会话应在启动时播种路径");
        assert!(!first.exists(), "惰性新会话：未追加前不落盘");

        h.requests.send(prompt("hi")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        assert_eq!(view.session_path.as_ref(), Some(&first));
        assert!(first.exists(), "回合结束后会话文件应落盘");
        assert_eq!(
            h.session_path.get().as_ref(),
            Some(&first),
            "普通回合不得换会话文件"
        );

        h.requests.send(Request::NewSession).await.unwrap();
        let _ = collect_until_view(&mut h).await;
        let second = h.session_path.get().expect("换会话后仍应有路径");
        assert_ne!(second, first, "/new 后快照应指向新会话文件");

        h.requests.send(prompt("next")).await.unwrap();
        let _ = collect_until_view(&mut h).await;
        assert_eq!(
            h.session_path.get().as_ref(),
            Some(&second),
            "快照只随 /new 变，不随回合变"
        );
    }

    /// 读会话文件首行 meta 的 `thinking_visible`（测试专用）。
    fn read_meta_thinking(path: &Path) -> bool {
        let content = std::fs::read_to_string(path).expect("会话文件应存在");
        let first = content.lines().next().expect("应有首行 meta");
        let meta: ys_session::SessionMeta = serde_json::from_str(first).expect("首行应为 meta");
        meta.thinking_visible
    }

    /// 读会话文件首行 meta 的 `effort`（测试专用）。
    fn read_meta_effort(path: &Path) -> Option<ys_core::EffortLevel> {
        let content = std::fs::read_to_string(path).expect("会话文件应存在");
        let first = content.lines().next().expect("应有首行 meta");
        let meta: ys_session::SessionMeta = serde_json::from_str(first).expect("首行应为 meta");
        meta.effort
    }

    /// 读会话文件首行 meta 的 `title`（测试专用）。
    fn read_meta_title(path: &Path) -> Option<String> {
        let content = std::fs::read_to_string(path).expect("会话文件应存在");
        let first = content.lines().next().expect("应有首行 meta");
        let meta: ys_session::SessionMeta = serde_json::from_str(first).expect("首行应为 meta");
        meta.title
    }

    /// **C3（TUI 路径）**：回合末写进 `meta.title` 的是 **skill 展开之前**的用户原文，
    /// 不是 `<skill …>` 信封（否则可达 9k 的正文会被当成标题）。
    ///
    /// 变异：把 `raw_user_text` 的取点挪到 `expand_skill_invocation` **之后** ⇒ 断言变红。
    #[tokio::test]
    async fn title_uses_pre_expansion_user_text() {
        let model = MockModel::new("mock");
        model.push_text("done");
        let mut h = spawn_app_loop_with(Box::new(model), true, Vec::new(), |config, tmp| {
            let root = tmp.join("skills");
            let dir = root.join("git-status");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                "---\nname: git-status\ndescription: 仓库状态\n---\n正文第一行\n正文第二行\n",
            )
            .unwrap();
            let (entries, _diags) = crate::skills::scan(&[root]);
            config.skills = entries;
            assert_eq!(config.skills.len(), 1, "测试前提：恰好扫到一条 skill");
        })
        .await;

        h.requests
            .send(prompt("/git-status 现在改了什么"))
            .await
            .unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        let path = view.session_path.expect("持久会话应有文件");

        let title = read_meta_title(&path).expect("回合末应写入 title");
        assert_eq!(title, "/git-status 现在改了什么");
        assert!(!title.contains("<skill"), "标题不得含展开信封：{title}");
    }

    /// `/thinking` 缓存随进程、**`/new` 不重置**（设计 §5.6 / T7）：`SetThinking{true}`
    /// 后经 `/new` 换会话，下一回合写出的 meta 仍为 `thinking_visible = true`。
    ///
    /// 变异：在 `Request::NewSession` 分支把缓存重置为 `false` → 第二条断言变红。
    #[tokio::test]
    async fn thinking_cache_survives_new_session() {
        let model = MockModel::new("mock");
        model.push_text("a");
        model.push_text("b");
        let mut h = spawn_app_loop(Box::new(model), true, Vec::new()).await;

        h.requests
            .send(Request::SetThinking { visible: true })
            .await
            .unwrap();
        let _ = collect_until_view(&mut h).await;

        h.requests.send(prompt("first")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        let first_path = view.session_path.clone().expect("持久会话应有文件");
        assert!(
            read_meta_thinking(&first_path),
            "回合末应把 thinking 写进 meta"
        );

        h.requests.send(Request::NewSession).await.unwrap();
        let _ = collect_until_view(&mut h).await;

        h.requests.send(prompt("second")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        let second_path = view.session_path.clone().expect("持久会话应有文件");
        assert_ne!(first_path, second_path, "应换到新会话");
        assert!(
            read_meta_thinking(&second_path),
            "/new 不得重置 thinking 缓存"
        );
    }

    /// **回合末把 `config.effort` 写进会话 meta**（TUI 路径）。
    ///
    /// 变异：把 `finalize_turn_meta` 调用点的 `Some(ctx.config.effort)` 改成 `None`
    /// ⇒ 回合结束后的 `meta.effort` 是 `None` ⇒ 本用例变红。
    #[tokio::test]
    async fn turn_end_persists_effort_to_meta() {
        let model = MockModel::new("mock");
        let mut h = spawn_app_loop_with(Box::new(model), true, Vec::new(), |config, _tmp| {
            // `/effort` 会重建 model；factory 给出可响应的实例，避免接线器被
            // 置空后回合拿不到模型。
            config.set_model_factory(|_| {
                let m = MockModel::new("effort-model");
                m.push_text("done");
                Some(Box::new(m))
            });
        })
        .await;

        h.requests
            .send(Request::SetEffort {
                effort: Some(ys_core::EffortLevel::High),
            })
            .await
            .unwrap();
        let _ = collect_until_view(&mut h).await;

        h.requests.send(prompt("hi")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        let path = view.session_path.expect("持久会话应有文件");
        assert_eq!(
            read_meta_effort(&path),
            Some(ys_core::EffortLevel::High),
            "回合末应把当前档位写进 meta"
        );
    }

    /// `/effort` 档位随进程、**`/new` 不重置**：`SetEffort(XHigh)` → 回合 →
    /// `/new` → 回合，新会话 meta 仍为 `Some(XHigh)`。
    ///
    /// 变异：在 `Request::NewSession` 分支把 `config.effort` 清成 `None` ⇒ 第二条
    /// 断言变红。
    #[tokio::test]
    async fn effort_survives_new_session() {
        let model = MockModel::new("mock");
        let mut h = spawn_app_loop_with(Box::new(model), true, Vec::new(), |config, _tmp| {
            // `/effort` 会重建 model；两回合各需一条响应。
            config.set_model_factory(|_| {
                let m = MockModel::new("effort-model");
                m.push_text("a");
                m.push_text("b");
                Some(Box::new(m))
            });
        })
        .await;

        h.requests
            .send(Request::SetEffort {
                effort: Some(ys_core::EffortLevel::XHigh),
            })
            .await
            .unwrap();
        let _ = collect_until_view(&mut h).await;

        h.requests.send(prompt("first")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        let first_path = view.session_path.clone().expect("持久会话应有文件");
        assert_eq!(
            read_meta_effort(&first_path),
            Some(ys_core::EffortLevel::XHigh),
            "回合末应把 effort 写进 meta"
        );

        h.requests.send(Request::NewSession).await.unwrap();
        let _ = collect_until_view(&mut h).await;

        h.requests.send(prompt("second")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        let second_path = view.session_path.clone().expect("持久会话应有文件");
        assert_ne!(first_path, second_path, "应换到新会话");
        assert_eq!(
            read_meta_effort(&second_path),
            Some(ys_core::EffortLevel::XHigh),
            "/new 不得重置 effort"
        );
    }

    /// 6. **`Request` 变体覆盖**：11 个变体中本用例发 9 个（`Prompt` + 7 个能力 +
    ///    `SetThinking`；`ListSessions` / `ResumeSession` 需额外前置条件，另有用例），
    ///    app 循环都必须处理并给出出站消息、不得 panic（`Prompt` → 事件；
    ///    7 个能力 → `Output` + `View`；`SetThinking` → 仅 `View`）。
    #[tokio::test]
    async fn every_request_variant_is_handled() {
        let model = MockModel::new("mock");
        model.push_text("hi");
        let mut h = spawn_app_loop(Box::new(model), true, Vec::new()).await;
        let export_path = h.temp_path("exported.jsonl");

        // 除 `Prompt` 外、会产出 `Output` 的 7 个变体（`SetThinking` 无输出，单列）。
        let cases = vec![
            Request::SetEffort {
                effort: Some(ys_core::EffortLevel::High),
            },
            Request::SetModel {
                model: "gpt-x".into(),
                provider: None,
            },
            Request::Login {
                provider: "custom".into(),
                api_key: "sk-test".into(),
                // `custom` 没有内置 base，harness 也未设 `YUSHAN_API_BASE` ⇒
                // 在参数校验处直接收束，不触发网络；登录成功/失败细节由
                // capabilities 的专门用例覆盖。本用例只验证变体分发与 Output。
                api_base: None,
            },
            Request::Logout,
            // `Export` 在 `NewSession` 之前：`/new` 惰性换出的新文件尚未落盘，
            // 导出它会（正确地）失败。
            Request::Export {
                path: Some(export_path.clone()),
            },
            Request::NewSession,
            Request::Compact,
        ];
        assert_eq!(cases.len(), 7, "7 个有输出的能力变体");

        // `Prompt` 先跑：会话有内容，`/export` 才有东西可导。
        h.requests.send(prompt("hello")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);
        assert!(
            msgs.iter().all(|m| matches!(m, Outbound::Event(_))),
            "Prompt 期间只应有事件：{msgs:?}"
        );

        for req in cases {
            let label = format!("{req:?}");
            h.requests.send(req).await.unwrap();
            let mut msgs = collect_until_view(&mut h).await;
            let _ = take_view(&mut msgs);
            match msgs.as_slice() {
                [Outbound::Output(_)] => {}
                other => panic!("{label} 的出站序列应恰为 [Output]：{other:?}"),
            }
        }

        assert!(
            export_path.exists(),
            "/export 应真的把会话文件复制到目标：{}",
            export_path.display()
        );

        // `SetThinking` 无输出：出站序列恰为 `[View]`。
        h.requests
            .send(Request::SetThinking { visible: true })
            .await
            .unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);
        assert!(msgs.is_empty(), "SetThinking 不应产出 Output：{msgs:?}");
    }

    /// `SetEffort` 的可见后果：视图里的档位随请求更新，`None` 清除回未设置。
    ///
    /// 变异：`app_loop` 里漏掉 `Request::SetEffort` 臂 ⇒ 编译失败（`match` 穷尽）；
    /// `view::build_view` 把 `CodingView { effort: cfg.effort }` 写死为 `None`
    /// （配置 → 视图的透传断掉）⇒ 第一条 `view.effort` 断言变红。
    #[tokio::test]
    async fn set_effort_updates_view() {
        let mut h = spawn_app_loop(Box::new(MockModel::new("mock")), false, Vec::new()).await;

        h.requests
            .send(Request::SetEffort {
                effort: Some(ys_core::EffortLevel::XHigh),
            })
            .await
            .unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        assert!(matches!(msgs.as_slice(), [Outbound::Output(_)]), "{msgs:?}");
        assert_eq!(view.effort, Some(ys_core::EffortLevel::XHigh));

        h.requests
            .send(Request::SetEffort { effort: None })
            .await
            .unwrap();
        let view = take_view(&mut collect_until_view(&mut h).await);
        assert_eq!(
            view.effort,
            Some(ys_core::EffortLevel::High),
            "None 清除会话覆盖后应回落到配置默认 high"
        );
    }

    /// 7. **UI 退出**：drop `request_tx` → app 循环在超时内**正常返回**（不挂起）。
    #[tokio::test]
    async fn dropping_request_sender_shuts_loop_down() {
        let model = MockModel::new("mock");
        model.push_text("bye");
        let mut h = spawn_with_mock(model).await;
        h.requests.send(prompt("hi")).await.unwrap();
        let _ = collect_until_view(&mut h).await;

        // 拆包，好把发送端 drop 掉（`Harness` 故意没有 `Drop`）。
        let Harness {
            requests,
            boundaries,
            models: _,
            outbound,
            mut handle,
            session_path: _,
            worktree: _,
            tmp,
        } = h;
        drop(requests);
        drop(boundaries);
        drop(outbound);

        let joined = tokio::time::timeout(TIMEOUT, &mut handle)
            .await
            .expect("请求信关闭后 app 循环应立即收摊（不得挂起）")
            .expect("app 循环任务不应 panic");
        assert!(joined.is_ok(), "正常收摊应为 Ok：{joined:?}");
        drop(tmp);
    }

    /// **窗口必须每回合刷新**（U6b）：`/login`、`/model` 能在运行中换 provider /
    /// model，窗口差可达 16 倍（128k ↔ 1M）。
    ///
    /// 观测手段是**自动压缩** —— 它是 `context_window` 唯一的可观测后果。本用例把
    /// model 的窗口经临时 `config.json` 压到 8000（触发点 `trigger_at(8000) = 2880`）：
    /// 会话养到 ≈7000 估算 token 后，第 2 个回合**必须**发 `ContextCompacted`。
    ///
    /// 变异：删掉 `Request::Prompt` 臂开头的 `agent.set_context_window(...)` →
    /// agent 停在 `RunLimits::default()` 的 128_000（触发点 108_800）→ 7000 远不触发
    /// → 本用例变红。
    #[tokio::test]
    async fn prompt_refreshes_context_window_each_turn() {
        let model = MockModel::new("mock");
        model.push_text("a".repeat(28_000)); // 回合 1 的回复：≈7000 token 的历史
        model.push_text("SUMMARY"); // 回合 2：压缩的摘要调用
        model.push_text("done"); // 回合 2：正式回复

        let mut h = spawn_app_loop_with(Box::new(model), false, Vec::new(), |config, tmp| {
            let path = tmp.join("config.json");
            std::fs::write(&path, r#"{"context_window": {"harness-model": 8000}}"#).unwrap();
            config.model = "harness-model".into();
            config.load_user_config(&path);
        })
        .await;

        // 回合 1：Step 4b 时 session 还只有本轮 user 消息（估算 0 token）⇒ 不压缩。
        h.requests.send(prompt("hi")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);
        assert!(
            !events_of(&msgs)
                .iter()
                .any(|e| matches!(e.event, AgentEvent::ContextCompacted { .. })),
            "第 1 回合的 session 还太小，不该压缩：{msgs:?}"
        );

        // 回合 2：窗口若已按 config 刷新为 8000 ⇒ 7000 > 2880 ⇒ 必须压缩。
        // （仍是默认 128_000 则触发点是 108_800，本回合不会压缩 → 断言变红。）
        h.requests.send(prompt("hi")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);
        assert!(
            events_of(&msgs)
                .iter()
                .any(|e| matches!(e.event, AgentEvent::ContextCompacted { .. })),
            "回合开头必须按 config 刷新窗口（8000）→ 触发自动压缩：{msgs:?}"
        );
    }

    /// 出站序列里出现了自动压缩吗 —— `context_window` 唯一的可观测后果。
    fn has_compaction(msgs: &[Outbound<CodingView>]) -> bool {
        events_of(msgs)
            .iter()
            .any(|e| matches!(e.event, AgentEvent::ContextCompacted { .. }))
    }

    /// **窗口必须每回合重算，而不是只在第一回合设一次**（U6b 的核心场景）。
    ///
    /// [`prompt_refreshes_context_window_each_turn`] 借「自动压缩是否触发」观测窗口
    /// 刷新，但那条用例里窗口是**常量**（8000）—— 「每回合重算」与「只在第一回合设
    /// 一次」产生的可观测结果**完全相同**，故它证明不了「每回合」。
    ///
    /// 本用例补上设计意图的核心场景：**运行中换到窗口差 16 倍的另一个 model**
    /// （`/model` / `/login` 的真实路径）。回合 1 用 128_000（触发点 108_800），
    /// ≈7000 token 的历史不动它；`SetModel` 换到 8000（触发点 2880）后，回合 2
    /// **必须**压缩。若窗口只在第一回合设过一次（或压根没设），回合 2 仍按
    /// 128_000 判定 → 不压缩 → 断言变红。
    ///
    /// 断言**刻意双向**：回合 1 的宽窗口下**不**压缩，回合 2 换窄窗口后**必须**压缩
    /// —— 只断言后半截的话，一个「永远压缩」的实现也能蒙混过关。
    ///
    /// 变异：删掉 `Request::Prompt` 臂开头的 `agent.set_context_window(...)` →
    /// agent 停在 `RunLimits::default()` 的 128_000 → 回合 2 不压缩 → 本用例变红。
    ///
    /// 换 model 会走 `capabilities::set_model` 里的 `config.build_model()` 重建后端，
    /// 故在 `tweak` 里装一个**脚本化的 model factory**（真实路径上是 OpenAI 兼容
    /// 适配器）—— 否则重建失败会把接线器的 model 置空，回合 2 直接 `ConfigError`，
    /// 那样即便实现正确也拿不到压缩事件（假红）。
    #[tokio::test]
    async fn context_window_is_recomputed_after_model_switch() {
        let model = MockModel::new("mock");
        model.push_text("a".repeat(28_000)); // 回合 1 的回复：≈7000 token 的历史

        let mut h = spawn_app_loop_with(Box::new(model), false, Vec::new(), |config, tmp| {
            let path = tmp.join("config.json");
            std::fs::write(
                &path,
                r#"{"context_window": {"harness-wide": 128000, "harness-narrow": 8000}}"#,
            )
            .unwrap();
            config.model = "harness-wide".into();
            config.load_user_config(&path);
            // 换 model 时 `build_model()` 重建的后端：给回合 2 备好「摘要 + 正式回复」。
            config.set_model_factory(|_| {
                let m = MockModel::new("mock-narrow");
                m.push_text("SUMMARY");
                m.push_text("done");
                Some(Box::new(m))
            });
        })
        .await;

        // 回合 1：窗口 128_000，会话里只有本轮 user 消息 ⇒ 不压缩。
        h.requests.send(prompt("hi")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);
        assert!(
            !has_compaction(&msgs),
            "回合 1 的窗口是 128_000，≈0 token 的会话不该压缩：{msgs:?}"
        );

        // 运行中换到窗口小 16 倍的 model —— 设计意图里的核心场景。
        h.requests
            .send(Request::SetModel {
                model: "harness-narrow".into(),
                provider: None,
            })
            .await
            .unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);

        // 回合 2：窗口若**每回合**重算 ⇒ 8000（触发点 2880）⇒ 7000 > 2880 ⇒ 必须压缩。
        // 若窗口只在回合 1 设过一次，这里仍是 128_000（触发点 108_800）⇒ 不压缩 → 变红。
        h.requests.send(prompt("hi")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);
        assert!(
            has_compaction(&msgs),
            "换 model 后回合 2 必须按新窗口（8000）重算 → 触发自动压缩：{msgs:?}"
        );
    }

    /// **空闲期丢弃残留边界消息**：循环开跑前就躺在边界信道里的 `Abort` 不得把
    /// 第一个回合取消。
    ///
    /// # 为什么要门控（它曾经是个时间赌博）
    ///
    /// 不加门控时 `select!` 的分支选择是**随机**的：`turn` 完全可能在
    /// `boundary_rx` 分支被评估之前就跑完（`MockModel` 一次 poll 即 Ready），
    /// 残留的 `Abort` 于是**碰巧**没被泵进本回合 —— 删掉排空逻辑也未必变红
    /// （实测：改坏后连跑 12 次只抓到 10 次）。
    ///
    /// 现在第 1 次 `complete` 被 gate 卡住：`turn` 在首次 poll 必然 `Pending`，
    /// `select!` 因此**必定**在同一趟 poll 里评估 `boundary_rx` 分支 ——
    /// 「残留 Abort 有没有泄漏进本回合」不再取决于调度顺序。
    ///
    /// 泄漏一旦发生，`BasicLoop` 会在回合入口探针（`rounds: 0`）或第 2 轮的
    /// 轮边界探针（`rounds: 1`）看到它，回合以 `Cancelled` 收场 ——
    /// [`ProbePlan::ToolCallThenFinal`] 特地为后者跨过一个轮边界。故断言 `Completed`。
    ///
    /// 变异：删掉 `run_turn_once` 开头的 `while ctx.boundary_rx.try_recv().is_ok() {}`
    /// → 本测试**确定性**变红（回合以 `Cancelled` 收场）。
    #[tokio::test]
    async fn stale_abort_queued_before_first_turn_is_discarded() {
        let (model, mut probe) = probe_model(ProbePlan::ToolCallThenFinal);
        let mut h = spawn_app_loop(Box::new(model), false, vec![Boundary::Abort]).await;

        h.requests.send(prompt("hi")).await.unwrap();

        // 等第 1 次 model 调用开始（此刻首次 select 已把所有分支评估完毕，残留
        // Abort 是否泄漏已成定局），然后放行。
        //
        // **等不到也无妨**：残留 Abort 若在回合入口就被命中，model 永不被调用，
        // 那时回合已经结束 —— 照样放行（多余的 permit 无害，model 不会再被调用），
        // 结论交给下面的断言。`ENTRY_WAIT` 只决定这条失败路径有多快，
        // **不会**把「等不到 model」变成另一种失败形态（那曾让失败信息变得难懂）。
        let _ = tokio::time::timeout(ENTRY_WAIT, probe.wait_entered(1)).await;
        probe.release();

        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);

        assert_eq!(
            stop_reasons(&events_of(&msgs)),
            vec![StopReason::Completed],
            "残留的 Abort 不该影响本回合：{msgs:?}"
        );
    }


    // ─────────────────── `/skill-name` 展开（T12，唯一接线点） ───────────────────

    /// **命中 `/<已知 skill>` ⇒ 正文进上下文**：Agent 侧第 1 轮的 `ModelRequest` 里
    /// 那条 user 消息必须含 SKILL.md 正文（`<skill …>` 信封形态），且**不含
    /// frontmatter**（`load_body` 已剥）；参数原样保留在尾部。
    ///
    /// 这条同时钉住**位置**：展开发生在 `turns += 1` / `run_turn` **之前** ——
    /// 若挪到 `run_turn` 之后（或压根没接），首轮请求里就只有原文 `/git-status …`。
    #[tokio::test]
    async fn prompt_with_known_skill_is_expanded_into_the_turn_input() {
        let (model, mut probe) = probe_model(ProbePlan::ToolCallThenFinal);
        let mut h = spawn_app_loop_with(Box::new(model), false, Vec::new(), |config, tmp| {
            let root = tmp.join("skills");
            let dir = root.join("git-status");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                "---\nname: git-status\ndescription: 仓库状态\n---\n正文第一行\n正文第二行\n",
            )
            .unwrap();
            config.load_skills(&[root]);
            assert_eq!(config.skills.len(), 1, "测试前提：恰好扫到一条 skill");
        })
        .await;

        h.requests
            .send(prompt("/git-status 现在改了什么"))
            .await
            .unwrap();
        probe.wait_entered(1).await;
        probe.release();

        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);

        let requests = probe.requests();
        assert!(!requests.is_empty(), "应至少有一次 model 调用");
        let first = &requests[0];
        assert!(
            has_text(first, "<skill name=\"git-status\""),
            "首轮请求必须已带上 skill 信封：{first:?}"
        );
        assert!(has_text(first, "正文第一行"), "{first:?}");
        assert!(has_text(first, "正文第二行"), "{first:?}");
        assert!(
            has_text(first, "现在改了什么"),
            "`/name` 之后的参数必须原样保留在尾部：{first:?}"
        );
        assert!(
            !has_text(first, "description:"),
            "正文已剥掉 frontmatter（`load_body` 的契约）：{first:?}"
        );
    }

    /// **负例逐字节不变**：非 skill 文本、未知命令、`/` 不在首位、无 skill 配置、
    /// 首块不是文本 —— 五种情形下消息**一字不改**。
    ///
    /// 变异：把 `expand_skill_invocation` 的 `None => message` 改成无条件重写
    /// （例如给原文套一层信封）⇒ 本用例逐条变红。
    #[test]
    fn non_skill_text_is_passed_through_verbatim() {
        use crate::skills::{Scope, SkillEntry};

        let root = temp_dir_for("expand");
        let dir = root.join("git-status");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\ndescription: 状态\n---\n正文\n").unwrap();
        let (skills, _) = crate::skills::scan(&[root.clone()]);
        assert_eq!(skills.len(), 1);

        for text in [
            "普通消息",
            "看看 /git-status 怎么用", // `/` 不在首位
            "/unknown-skill 参数",     // 不命中
            "git-status",              // 无前导 `/`
            "/",                       // 光一个斜杠
        ] {
            let msg = user_message(text);
            assert_eq!(
                expand_skill_invocation(msg.clone(), &skills),
                msg,
                "非 skill 调用必须逐字节不变：{text:?}"
            );
        }

        // 有 skill 表但**没有任何 skill**（启动期扫描为空）⇒ 同样不变。
        let msg = user_message("/git-status");
        assert_eq!(expand_skill_invocation(msg.clone(), &[]), msg);

        // 首块不是 `Text` ⇒ 不展开（只认纯文本首块的 `/` 命名空间）。
        let tool_first = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: ToolCallId("c1".into()),
                content: "/git-status".into(),
                is_error: false,
            }],
        };
        assert_eq!(
            expand_skill_invocation(tool_first.clone(), &skills),
            tool_first
        );

        // `user-invocable: false` 的 skill：`/` 不认（`expand_invocation` 的契约）。
        let model_only = SkillEntry {
            command: "model-only".into(),
            label: "model-only".into(),
            description: "只给模型".into(),
            path: root.join("model-only/SKILL.md"),
            scope: Scope::User,
            model_visible: true,
            user_invocable: false,
        };
        let msg = user_message("/model-only");
        assert_eq!(expand_skill_invocation(msg.clone(), &[model_only]), msg);

        let _ = std::fs::remove_dir_all(&root);
    }

    // ────────── wall-clock 预算 + §4.9 可观测性（ADR-0027 决策 6） ──────────

    /// **预算时长来自 config、到点即中止**（本轮「可配」的验收点）。
    ///
    /// 临时 `config.json` 写 `max_turn_duration_secs: 1` 并经 `load_user_config`
    /// 加载 ⇒ `config.max_turn_duration()` = 1s。这正是「时长来自 config 而非写死」
    /// 的验收点：若把时长写死成默认 48h，本用例等不到说明行 ⇒ 超时变红。
    ///
    /// 探针 `AlwaysToolCall` + 第 1 次 model 调用被 gate 卡住 ⇒ 回合主动停留 > 1s，
    /// 计时器得以在回合**进行中**到点：先发一条含 `budget exhausted` 的 `Output`，
    /// 再向本回合边界源 push `Abort`。放行 gate 后回合在轮边界看到 `Abort` ⇒
    /// `Cancelled`。
    #[tokio::test]
    async fn wall_clock_budget_exhaustion_aborts_turn_with_output() {
        let (model, mut probe) = probe_model(ProbePlan::AlwaysToolCall);
        let mut h = spawn_app_loop_with(Box::new(model), false, Vec::new(), |config, tmp| {
            let path = tmp.join("config.json");
            std::fs::write(&path, r#"{"max_turn_duration_secs": 1}"#).unwrap();
            config.load_user_config(&path);
            assert_eq!(
                config.max_turn_duration(),
                Duration::from_secs(1),
                "测试前提：预算时长来自 config.json"
            );
        })
        .await;

        h.requests.send(prompt("hi")).await.unwrap();
        probe.wait_entered(1).await; // 回合此刻卡在第 1 次 model 调用上

        // 等预算到点：app 应在回合**仍进行中**发说明行（证明计时器与回合并发，而非
        // 等回合结束后才补账）。**有界等待**（预算 1s，给 4s 余量）：收不到说明行
        // （变异：时长写死 / 漏发这一行）也**不 panic**，落到底部断言统一报红。
        let mut msgs = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                let m = h.outbound.recv().await.expect("app 循环不应提前收摊");
                let hit = matches!(&m, Outbound::Output(t) if t.contains("budget exhausted"));
                msgs.push(m);
                if hit {
                    break;
                }
            }
        })
        .await;
        // 让 app 任务把 `Abort` push 进本回合边界源，再放行被 gate 卡住的调用。
        probe.let_app_task_pump().await;
        probe.release();
        msgs.extend(collect_until_view(&mut h).await);
        let _ = take_view(&mut msgs);

        assert_eq!(
            msgs.iter()
                .filter(|m| matches!(m, Outbound::Output(t) if t.contains("budget exhausted")))
                .count(),
            1,
            "预算到点应**恰有一条**说明行：{msgs:?}"
        );
        // 钉住说明行里的**时长值** = `config.max_turn_duration().as_secs()`（此处 config.json
        // 写的是 1s）。若把时长写死成另一个「足够短但错误」的值（如 2s），它仍会在用例窗口内
        // 到点、仍含 "budget exhausted"，但本条断言会红。
        let budget_line = msgs
            .iter()
            .find_map(|m| match m {
                Outbound::Output(t) if t.contains("budget exhausted") => Some(t.as_str()),
                _ => None,
            })
            .expect("应有一条 budget exhausted 说明行");
        assert!(
            budget_line.contains("of 1s"),
            "说明行须含配置的时长值 1s（来自 config.max_turn_duration()）：{budget_line:?}"
        );
        assert_eq!(
            stop_reasons(&events_of(&msgs)),
            vec![StopReason::Cancelled],
            "预算耗尽应以 Cancelled 收场：{msgs:?}"
        );
    }

    /// **`max_rounds` = `u32::MAX` ⇒ 不被核心默认 10 截断**（ADR-0027 决策 1）。
    ///
    /// **接线**：`app_loop` 每回合构造 `AppSubagentRunner` 并经 `AgentPorts` 注入 ⇒
    /// 模型发一个 `subagent` 工具调用时，它真的在环里跑通（子 agent 产出 → 结果回喂父模型）。
    ///
    /// 这是唯一覆盖「注入那几行」的用例：`subagent` 单元测试直接调 runner，跳过了
    /// `TurnCtx.subagent → make_runner → AgentPorts::with_subagent` 这条链。
    #[tokio::test]
    async fn subagent_runner_is_injected_each_turn() {
        let model = ys_model::MockModel::new("m");
        // 父第 1 轮：发一个 `subagent` 调用。
        model.push_tool_call(
            "subagent",
            serde_json::json!({"description": "查 X", "prompt": "查 X"}),
        );
        // 子 agent 的唯一一次模型调用，吃掉下一条响应。
        model.push_text("CHILD-OUTPUT");
        // 父第 2 轮（看到工具结果后）收尾。
        model.push_text("parent done");

        let mut h = spawn_app_loop_full(
            Box::new(model),
            false,
            Vec::new(),
            |_, _| {},
            |b| {
                b.tools(vec![Box::new(crate::subagent::SubagentTool::new(
                    std::time::Duration::from_secs(30),
                ))])
            },
            |tmp| {
                Some(crate::subagent::SubagentSetup::shared(
                    "你是子 agent".to_string(),
                    ys_component::RunLimits::new(3),
                    std::time::Duration::from_secs(30),
                    tmp.to_path_buf(),
                    WebConfig::offline(FetchedDir::Temp(tmp.join("fetched"))),
                    // 产物目录注入临时路径 —— 不写进用户的 `~/.yushan/subagents/`。
                    Arc::new(crate::subagent::SubagentsDir::at(
                        tmp.join("subagents"),
                        false,
                    )),
                ))
            },
            None,
            false,
            false,
        )
        .await;

        h.requests.send(prompt("hi")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);

        let contents: Vec<String> = events_of(&msgs)
            .into_iter()
            .filter_map(|env| match &env.event {
                AgentEvent::ToolResult { result, .. } => Some(result.content.clone()),
                _ => None,
            })
            .collect();
        assert!(
            contents.iter().any(|c| c.contains("CHILD-OUTPUT")),
            "子 agent 的产出必须作为工具结果回喂（注入链未接通？）：{contents:?}"
        );
        assert!(
            contents.iter().any(|c| c.contains("## Files")),
            "runner 必须保证产物节存在：{contents:?}"
        );
    }

    /// 生产侧 `resolve_limits`（`main.rs`）把 `config.max_rounds()`（app 默认
    /// `u32::MAX`，= 不设）装进 `RunLimits.max_rounds`；本用例在测试台重现该装配。
    /// 探针 `CompleteAtCalls(&[12])` 需要跑满 12 轮才收尾 —— 若限制仍是核心默认 10，
    /// 回合会在第 10 轮以 `MaxRounds` 收场 ⇒ 断言变红。
    #[tokio::test]
    async fn max_rounds_above_core_default_runs_to_completion() {
        let (model, mut probe) = probe_model(ProbePlan::CompleteAtCalls(&[12]));
        let mut h = spawn_app_loop_full(
            Box::new(model),
            false,
            Vec::new(),
            |_, _| {},
            |b| b.limits(ys_component::RunLimits::new(u32::MAX)),
            |_| None,
            None,
            false,
            false,
        )
        .await;

        h.requests.send(prompt("hi")).await.unwrap();
        probe.wait_entered(1).await;
        probe.release();

        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);

        let events = events_of(&msgs);
        // 主回合必须 `Completed`（而非被核心默认 10 截断的 `MaxRounds`）。
        assert_eq!(
            stop_reasons(&events).first(),
            Some(&StopReason::Completed),
            "max_rounds = u32::MAX 时主回合须 Completed，不得被核心默认截断：{msgs:?}"
        );
        assert!(
            !stop_reasons(&events).contains(&StopReason::MaxRounds),
            "不得出现 MaxRounds：{msgs:?}"
        );
        // 主回合实际跑到的轮数 > 10 —— 直接证明未被核心默认 10 截断。
        let main_rounds = events
            .iter()
            .find_map(|e| match &e.event {
                AgentEvent::RunFinished { rounds, .. } => Some(*rounds),
                _ => None,
            })
            .expect("应有 RunFinished");
        assert!(
            main_rounds > 10,
            "主回合应跑过 10 轮（证明未被核心默认 10 截断），实际 {main_rounds} 轮"
        );
    }

    /// 临时目录（含进程内原子序号 + pid；调用方负责删除）。
    fn temp_dir_for(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "yushan_app_loop_skill_{tag}_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ─────────────────────── 运行期 `/resume`（U5 / T12） ───────────────────────

    /// 持久 harness + 「可恢复」的配置：env 凭证齐备（`/resume` 的模型解析走档 1
    /// 短路）+ 一个**每次造全新 `MockModel`** 的 factory（候选 model 由它构建）。
    /// `replies` 给每次 factory 调用造出的 MockModel 压入若干 `"done"` 文本 ——
    /// 切换后要跑回合的用例给 `>= 1`。
    ///
    /// **初始 model 也预压一条 `"initial"` 文本**：用例若在 `/resume` **之前**先跑
    /// 一个回合（进程回合序从 1 起），需要它可被完整跑完 —— `MockModel` 无预写响应
    /// 时 `complete` 的 `remove(0)` 会 panic。仅跑 `/resume`（不先跑回合）的既有
    /// 用例不受影响（初始 model 不会被调用）。
    async fn spawn_resumable(replies: usize) -> Harness {
        let initial = MockModel::new("initial");
        initial.push_text("initial");
        spawn_app_loop_with(Box::new(initial), true, Vec::new(), move |config, _tmp| {
            config.api_base = Some("http://resume-base/v1".into());
            config.api_key = Some("sk-resume".into());
            config.set_model_factory(move |_| {
                let m = MockModel::new("resumed");
                for _ in 0..replies {
                    m.push_text("done");
                }
                Some(Box::new(m))
            });
        })
        .await
    }

    /// 往 harness 的 `sessions/` 里落一个会话文件（首行 meta + 若干消息）。
    fn write_target_session(
        h: &Harness,
        id: &str,
        meta: &ys_session::SessionMeta,
        msgs: &[Message],
    ) -> PathBuf {
        let dir = h.tmp.join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        let mut content = format!("{}\n", serde_json::to_string(meta).unwrap());
        for m in msgs {
            content.push_str(&format!("{}\n", serde_json::to_string(m).unwrap()));
        }
        std::fs::write(&path, content).unwrap();
        path
    }

    /// **N3（I2）运行期出站顺序**：`SessionResumed` → `Output` → `View`。
    ///
    /// 变异：把 `Request::ResumeSession` 臂的 `session_event` 移到 `Output` 之后
    /// 投递 ⇒ 本用例断言变红（且真实 UI 侧回放的清屏会吃掉那条提示）。
    #[tokio::test]
    async fn resume_emits_session_resumed_then_output_then_view() {
        let mut h = spawn_resumable(0).await;
        let mut meta = ys_session::SessionMeta::new(0);
        meta.title = Some("目标会话".into());
        let dangling = Message {
            role: ys_core::Role::Assistant,
            content: vec![ys_core::ContentBlock::ToolUse {
                id: ToolCallId("call-1".into()),
                name: "read".into(),
                arguments: serde_json::json!({"path": "src/lib.rs"}),
            }],
        };
        write_target_session(&h, "target-1", &meta, &[user_message("hello"), dangling]);

        h.requests
            .send(Request::ResumeSession {
                target: "target-1".into(),
            })
            .await
            .unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);

        assert_eq!(msgs.len(), 2, "应恰为 SessionResumed + Output：{msgs:?}");
        match &msgs[0] {
            Outbound::SessionResumed {
                summary,
                thinking_visible,
                history,
            } => {
                assert_eq!(summary.id, "target-1");
                assert_eq!(summary.title, "目标会话");
                assert!(!*thinking_visible, "目标 meta 未开 thinking");
                assert_eq!(history.len(), 3, "应带目标历史与合成结果");
                assert!(matches!(
                    &history[2].content[..],
                    [ys_core::ContentBlock::ToolResult {
                        tool_call_id,
                        is_error: true,
                        ..
                    }] if tool_call_id == &ToolCallId("call-1".into())
                ));
            }
            other => panic!("第一条出站必须是 SessionResumed（I2）：{other:?}"),
        }
        assert!(
            matches!(&msgs[1], Outbound::Output(t)
                if t.contains("Resumed session")
                    && t.contains("1 interrupted tool call")),
            "第二条应为成功提示 + 修复摘要 Output：{msgs:?}"
        );
    }

    /// **决策 5（成功路径）**：`/resume` **成功**后进程级回合序 `turns`（`Envelope.turn`
    /// 的驱动）**不归零**；而 `view.turn_count` 由目标 meta 的 `turns` 重新播种 ——
    /// 两者**不同源**、刻意分叉。
    ///
    /// 既有用例 `resume_without_sessions_dir_prompts_and_turns_not_reset` 钉的是
    /// **内存会话**（`/resume` 必失败）⇒ `if let Some(r) = resumed` 的**成功块从不
    /// 执行**，本决策实际无保护。本用例走**持久 harness + 真实会话文件**，让成功块
    /// 真正执行。
    ///
    /// 断言：回合 1 `Envelope.turn == 1`；成功 `/resume` 发 `SessionResumed` 且
    /// `view.turn_count` 由目标 meta（`turns = 7`）播种；回合 2 `Envelope.turn == 2`
    /// （不归零），`view.turn_count == 8`（目标 meta 7 + 本回合）—— 与 `Envelope.turn`
    /// 数字不同，证两者异源。
    ///
    /// 变异：在成功块里插入 `turns = 0;`（`stats = seed_stats(...)` 之前）⇒ 回合 2 的
    /// `Envelope.turn` 变 1 ⇒ 本用例第二条 `turns` 断言变红。
    #[tokio::test]
    async fn resume_success_keeps_process_turn_sequence_but_seeds_view_from_target_meta() {
        let mut h = spawn_resumable(1).await;

        // 回合 1：进程回合序 = 1。
        h.requests.send(prompt("one")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        assert_eq!(view.turn_count, 1);
        let turns: Vec<u32> = events_of(&msgs).iter().map(|e| e.turn).collect();
        assert!(
            !turns.is_empty() && turns.iter().all(|&t| t == 1),
            "回合 1 的 Envelope.turn 应为 1：{turns:?}"
        );

        // 目标会话：meta.turns = 7（**刻意 ≠ 进程回合序**）⇒ 播种后 view.turn_count = 7。
        let mut meta = ys_session::SessionMeta::new(0);
        meta.turns = 7;
        write_target_session(&h, "target-meta", &meta, &[user_message("prior")]);

        // **成功** `/resume` ⇒ 必须发出 `SessionResumed`（证明走的是成功块）。
        h.requests
            .send(Request::ResumeSession {
                target: "target-meta".into(),
            })
            .await
            .unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        assert!(
            matches!(msgs.first(), Some(Outbound::SessionResumed { .. })),
            "成功 /resume 必须发 SessionResumed：{msgs:?}"
        );
        assert_eq!(
            view.turn_count, 7,
            "view.turn_count 应由目标 meta.turns 播种"
        );

        // 回合 2：`turns` 不归零 ⇒ Envelope.turn = 2；`stats` 在 7 上 +1 ⇒ 8。
        h.requests.send(prompt("two")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        let turns: Vec<u32> = events_of(&msgs).iter().map(|e| e.turn).collect();
        assert!(
            !turns.is_empty() && turns.iter().all(|&t| t == 2),
            "进程回合序不因 /resume 归零：Envelope.turn 应为 2，得到 {turns:?}"
        );
        assert_eq!(
            view.turn_count, 8,
            "view.turn_count 走目标 meta(7) + 本回合 ⇒ 与 Envelope.turn(2) 不同源"
        );
    }

    /// **N7（I9）thinking 镜像变异**：目标会话 `T_target = true`、当前（新）会话
    /// 无 meta ⇒ `T_old = false`。切换后跑**一个回合**，断言**目标文件 meta** 的
    /// `thinking_visible == T_target`。
    ///
    /// 变异：删掉 `Request::ResumeSession` 臂里的 `thinking_visible = r.thinking_visible`
    /// ⇒ 镜像仍是旧值 false ⇒ 下一回合把 false 写进目标 meta ⇒ 本用例变红。
    #[tokio::test]
    async fn resume_resets_thinking_mirror_so_next_turn_writes_target_value() {
        let mut h = spawn_resumable(1).await;
        let mut meta = ys_session::SessionMeta::new(0);
        meta.thinking_visible = true; // T_target = true ≠ T_old（新会话默认 false）
        let path = write_target_session(&h, "target-think", &meta, &[]);

        h.requests
            .send(Request::ResumeSession {
                target: "target-think".into(),
            })
            .await
            .unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);

        // 切换后跑一个回合 —— 回合末无条件把进程级镜像写进**目标** meta。
        h.requests.send(prompt("hi")).await.unwrap();
        let _ = collect_until_view(&mut h).await;

        assert!(
            read_meta_thinking(&path),
            "切换后第一回合必须把**目标**的 thinking(true) 写进目标 meta \
             —— 漏了镜像重设就会写回旧会话的 false"
        );
    }

    /// **I4**：空列表**照发** `SessionList(vec![])`（UI 据此提示，不弹空浮层）。
    #[tokio::test]
    async fn list_sessions_sends_empty_list_when_dir_empty() {
        let mut h = spawn_resumable(0).await;
        // 启动的新会话是**惰性**的：未追加前 sessions 目录为空。

        h.requests.send(Request::ListSessions).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);

        match msgs.as_slice() {
            [Outbound::SessionList(items)] => assert!(items.is_empty(), "空列表照发（I4）"),
            other => panic!("应发 SessionList(vec![])：{other:?}"),
        }
    }

    /// `/resume` 无参：目录里有会话时列出它（id + 标题）。
    #[tokio::test]
    async fn list_sessions_returns_existing_sessions() {
        let mut h = spawn_resumable(0).await;
        let mut meta = ys_session::SessionMeta::new(0);
        meta.title = Some("可恢复".into());
        write_target_session(&h, "listed-1", &meta, &[]);

        h.requests.send(Request::ListSessions).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);

        match msgs.as_slice() {
            [Outbound::SessionList(items)] => {
                assert_eq!(items.len(), 1, "{items:?}");
                assert_eq!(items[0].id, "listed-1");
                assert_eq!(items[0].title, "可恢复");
            }
            other => panic!("应发 SessionList：{other:?}"),
        }
    }

    /// **N11（I13 + 决策 5）**：内存会话（`sessions_dir == None`）⇒
    /// `/resume` 与列表请求**只有** `Output`、**不发** `SessionList` / `SessionResumed`；
    /// 且 `Envelope.turn` **不归零**（切换/内存会话不重置进程级回合序）。
    #[tokio::test]
    async fn resume_without_sessions_dir_prompts_and_turns_not_reset() {
        let model = MockModel::new("m");
        model.push_text("a");
        model.push_text("b");
        let mut h = spawn_app_loop(Box::new(model), false, Vec::new()).await;

        // 回合 1（进程回合序 = 1）。
        h.requests.send(prompt("one")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let v = take_view(&mut msgs);
        assert_eq!(v.turn_count, 1);

        // 列表：内存会话 ⇒ 只有 Output，无 SessionList（I13）。
        h.requests.send(Request::ListSessions).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);
        assert!(
            matches!(msgs.as_slice(), [Outbound::Output(_)]),
            "内存会话不得发 SessionList：{msgs:?}"
        );

        // 恢复：内存会话 ⇒ 只有 Output，无 SessionResumed。
        h.requests
            .send(Request::ResumeSession {
                target: "whatever".into(),
            })
            .await
            .unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);
        assert!(
            matches!(msgs.as_slice(), [Outbound::Output(_)]),
            "内存会话不得发 SessionResumed：{msgs:?}"
        );

        // 回合 2：`turns` 不因上面的请求归零 —— 仍递增到 2。
        h.requests.send(prompt("two")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let v = take_view(&mut msgs);
        assert_eq!(v.turn_count, 2, "turns 是进程级回合序，不因切换归零");
        let turns: Vec<u32> = events_of(&msgs).iter().map(|e| e.turn).collect();
        assert!(turns.iter().all(|&t| t == 2), "{turns:?}");
    }

    // ─────────── 会话内 `/resume` 换工作区（U4）：工具 / 提示词 / meta / 退出行一致 ───────────

    /// 落一条带 `workspace` 记录的目标会话（首行 meta + 一条历史）。
    fn write_target_with_workspace(h: &Harness, id: &str, workspace: Option<PathBuf>) -> PathBuf {
        let mut meta = ys_session::SessionMeta::new(0);
        meta.workspace = workspace;
        write_target_session(h, id, &meta, &[user_message("prior")])
    }

    /// 读会话文件首行 meta 的 `workspace`（测试专用）。
    fn read_meta_workspace(path: &Path) -> Option<PathBuf> {
        let content = std::fs::read_to_string(path).expect("会话文件应存在");
        let first = content.lines().next().expect("应有首行 meta");
        let meta: ys_session::SessionMeta = serde_json::from_str(first).expect("首行应为 meta");
        meta.workspace
    }

    /// 把出站序列里的 `Output` 行拼起来（断言提示文案）。
    fn output_text(msgs: &[Outbound<CodingView>]) -> String {
        msgs.iter()
            .filter_map(|m| match m {
                Outbound::Output(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `/resume` 能重建 model 的 harness：env 凭证齐备 + 一个「每次造全新 `MockModel`」的
    /// factory；`script` 决定该 MockModel 的脚本化响应（切换后要跑的回合由它驱动）。
    /// `binding` = 启动期工作区绑定。
    async fn spawn_for_workspace(
        binding: Option<WorktreeBinding>,
        script: impl Fn(&MockModel) + Send + Sync + 'static,
    ) -> Harness {
        spawn_app_loop_full(
            Box::new(MockModel::new("initial")),
            true,
            Vec::new(),
            move |config, _tmp| {
                config.api_base = Some("http://resume-base/v1".into());
                config.api_key = Some("sk-resume".into());
                config.set_model_factory(move |_| {
                    let m = MockModel::new("resumed");
                    script(&m);
                    Some(Box::new(m))
                });
            },
            |b| b,
            |_| None,
            binding,
            false,
            false,
        )
        .await
    }

    /// **① 记录存在且在 ⇒ 切过去**，且**工具工作区 / `CodingView.cwd` / meta / 句柄**四者一致。
    ///
    /// 工具工作区的观测：切换后跑一个回合，让模型发一个 `write` 相对路径调用 —— 落点由
    /// **工具构造期捕获的工作区**决定，故文件只会落在新树里。
    ///
    /// 变异：删掉 `Request::ResumeSession` 臂里的 `apply_resumed_workspace` 调用 ⇒
    /// `view.cwd` 仍是共享工作区、`probe.txt` 落错地方 ⇒ 多条断言变红。
    #[tokio::test]
    async fn resume_switches_to_recorded_workspace_consistently() {
        let mut h = spawn_for_workspace(None, |m| {
            m.push_tool_call(
                "write",
                serde_json::json!({"path": "probe.txt", "content": "hello"}),
            );
            m.push_text("done");
        })
        .await;

        let shared = h.tmp.0.clone();
        let tree = make_tree(&h, "tree-b");
        let target = write_target_with_workspace(&h, "target-b", Some(tree.clone()));

        h.requests
            .send(Request::ResumeSession {
                target: "target-b".into(),
            })
            .await
            .unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);

        // I2：SessionResumed 在最前；告知切树在 Output 里。
        assert!(
            matches!(msgs.first(), Some(Outbound::SessionResumed { .. })),
            "SessionResumed 必须排在最前（I2）：{msgs:?}"
        );
        let out = output_text(&msgs);
        assert!(
            out.contains("已恢复到工作树：tree-b"),
            "应告知恢复到工作树：{out}"
        );
        // system prompt 的 cwd（经视图透出）已是新树。
        assert_eq!(view.cwd, tree, "视图 cwd 应切到新工作树");
        // 工作区句柄刷成新树（退出行 / 回合末 meta 的取数口）。
        let binding = h.worktree.get().expect("应绑定到 tree-b");
        assert_eq!(binding.path, tree);
        assert_eq!(binding.name, "tree-b");

        // 工具的工作区：跑一个回合，`write` 相对落点只可能在新树里。
        h.requests.send(prompt("go")).await.unwrap();
        let _ = collect_until_view(&mut h).await;
        assert!(
            tree.join("probe.txt").is_file(),
            "write 应落在新工作区：{tree:?}"
        );
        assert!(
            !shared.join("probe.txt").exists(),
            "不得落在共享工作区：{shared:?}"
        );

        // meta 记录新工作区（与句柄同源）。
        assert_eq!(
            read_meta_workspace(&target).as_deref(),
            Some(tree.as_path()),
            "回合末 meta.workspace 应记录新工作树"
        );
    }

    /// **② 记录存在但树已不在 ⇒ 告知 + 保持当前工作区**（不重建）。
    ///
    /// 变异：把 `Some(p)` 缺失分支也当成「切到 p」⇒ 重建会失败、`view.cwd` 变成不存在的
    /// 路径、且会打「已恢复到工作树」⇒ 断言变红。
    #[tokio::test]
    async fn resume_missing_recorded_workspace_keeps_current() {
        let mut h = spawn_for_workspace(None, |m| m.push_text("done")).await;
        let shared = h.tmp.0.clone();
        // 记录一棵**不存在**的树。
        let ghost = h.tmp.join("gone-tree");
        let _ = write_target_with_workspace(&h, "target-ghost", Some(ghost.clone()));

        h.requests
            .send(Request::ResumeSession {
                target: "target-ghost".into(),
            })
            .await
            .unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);

        let out = output_text(&msgs);
        assert!(
            out.contains("记录的工作树已不存在"),
            "应明确告知记录的工作树不存在：{out}"
        );
        assert!(
            !out.contains("已恢复到工作树"),
            "缺失树不得声称已恢复：{out}"
        );
        assert_eq!(view.cwd, shared, "缺失树时保持当前（共享）工作区");
        assert!(h.worktree.get().is_none(), "缺失树时绑定不变（仍为共享）");
    }

    /// **③ 无记录 + 当前在工作树 ⇒ 切回共享工作区**并告知。
    ///
    /// 变异：`None` 分支也当成「不动」⇒ `view.cwd` 仍是旧树 ⇒ 断言变红。
    #[tokio::test]
    async fn resume_without_recorded_workspace_returns_to_shared() {
        let tree_dir = TempDir::new("tree_a");
        let binding = WorktreeBinding {
            name: "tree-a".into(),
            path: tree_dir.0.clone(),
        };
        let mut h = spawn_for_workspace(Some(binding), |m| m.push_text("done")).await;
        // 目标会话**无**工作区记录。
        let _ = write_target_with_workspace(&h, "no-ws", None);

        h.requests
            .send(Request::ResumeSession {
                target: "no-ws".into(),
            })
            .await
            .unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);

        let out = output_text(&msgs);
        assert!(
            out.contains("已切回共享工作区"),
            "应告知切回共享工作区：{out}"
        );
        assert_eq!(view.cwd, h.tmp.0, "应切回共享工作区");
        assert!(
            h.worktree.get().is_none(),
            "共享工作区 ⇒ 绑定清空（退出行不再打 worktree:）"
        );
    }

    /// **④ 无记录 + 当前已在共享工作区 ⇒ 不动、不提示**（不制造噪声）。
    ///
    /// 变异：`None` 分支无条件重建 / 提示 ⇒ 断言变红。
    #[tokio::test]
    async fn resume_without_workspace_stays_in_shared_silently() {
        let mut h = spawn_for_workspace(None, |m| m.push_text("done")).await;
        let _ = write_target_with_workspace(&h, "no-ws", None);

        h.requests
            .send(Request::ResumeSession {
                target: "no-ws".into(),
            })
            .await
            .unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);

        let out = output_text(&msgs);
        assert!(
            !out.contains("工作树") && !out.contains("共享工作区"),
            "无记录且已在共享 ⇒ 不得打工作区切换提示：{out}"
        );
        assert_eq!(view.cwd, h.tmp.0);
        assert!(h.worktree.get().is_none());
    }

    /// **⑤ 记录与当前是同一棵树 ⇒ 不重建、不提示**（`/resume` 到同一棵树的会话）。
    ///
    /// 变异：去掉 `same_dir` 短路 ⇒ 无条件重建并打「已恢复到工作树」⇒ 断言变红。
    #[tokio::test]
    async fn resume_same_recorded_workspace_is_a_noop() {
        let tree_dir = TempDir::new("tree_a_same");
        let binding = WorktreeBinding {
            name: "tree-a".into(),
            path: tree_dir.0.clone(),
        };
        let mut h = spawn_for_workspace(Some(binding), |m| m.push_text("done")).await;
        let _ = write_target_with_workspace(&h, "same-ws", Some(tree_dir.0.clone()));

        h.requests
            .send(Request::ResumeSession {
                target: "same-ws".into(),
            })
            .await
            .unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);

        let out = output_text(&msgs);
        assert!(
            !out.contains("已恢复到工作树"),
            "同一棵树不得重复提示：{out}"
        );
        assert_eq!(view.cwd, tree_dir.0, "cwd 保持在该树");
        assert_eq!(
            h.worktree.get().expect("绑定应保持").path,
            tree_dir.0,
            "绑定保持在该树"
        );
    }

    /// 建一个真实子目录当「工作树」（在 harness 临时目录之下）。
    fn make_tree(h: &Harness, name: &str) -> PathBuf {
        let dir = h.tmp.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ───────────────────────── debug 接线（Task 16）─────────────────────────

    /// 测试期 env 覆盖（`YUSHAN_LOG_DIR` + `YUSHAN_DEBUG`），**整段持有 `env_lock`**。
    ///
    /// 持有锁时必须把 harness 的 `env_prelocked` 传 `true` —— 它内部不再重复加锁
    /// （`Mutex` 不可重入，同线程二次加锁会死锁）。
    ///
    /// 字段顺序即 drop 顺序：先恢复环境（此时锁仍活着），再释放锁。
    struct DebugEnv {
        _restore: crate::test_env::EnvRestore,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl DebugEnv {
        fn set(log_dir: &Path) -> Self {
            let guard = crate::test_env::env_lock();
            let restore = crate::test_env::EnvRestore::capture(&[
                crate::logging::LOG_DIR_ENV,
                crate::debug_log::DEBUG_ENV,
            ]);
            // SAFETY: 持有 env_lock。
            unsafe {
                std::env::set_var(crate::logging::LOG_DIR_ENV, log_dir);
                std::env::remove_var(crate::debug_log::DEBUG_ENV);
            }
            Self {
                _restore: restore,
                _guard: guard,
            }
        }
    }

    /// 目录里的 `debug-*.jsonl`：返回 `(路径, 解析后的记录)`，按路径排序。
    fn debug_files(dir: &Path) -> Vec<(PathBuf, Vec<serde_json::Value>)> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .map(|name| {
                        let name = name.to_string_lossy();
                        name.starts_with("debug-") && name.ends_with(".jsonl")
                    })
                    .unwrap_or(false)
            })
            .collect();
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                let content = std::fs::read_to_string(&path).unwrap();
                let records = content
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                (path, records)
            })
            .collect()
    }

    fn record_evs(records: &[serde_json::Value]) -> Vec<&str> {
        records
            .iter()
            .map(|record| record["ev"].as_str().expect("每条记录都有 ev"))
            .collect()
    }

    /// 正常收摊：drop 请求发送端 → 等 app 循环返回（`flush` 在此路径上执行）。
    async fn shutdown(mut h: Harness) {
        drop(h.requests);
        let joined = tokio::time::timeout(TIMEOUT, &mut h.handle)
            .await
            .expect("drop requests 后 app 循环应立即收摊")
            .expect("app 循环任务不应 panic");
        assert!(joined.is_ok(), "正常收摊应为 Ok：{joined:?}");
    }

    /// TUI 层：debug 开时一个回合落齐 `turn_start / round_start / tool_call /
    /// round_end / turn_end`；`yushan.log` 的既有文本行形状不变。
    ///
    /// 变异：事件泵里跳过 `debug.record`（例如把 `observe_debug` 换回 `observe`）
    /// ⇒ `round_start` / `tool_call` / `round_end` 缺失 ⇒ 红。
    #[tokio::test]
    async fn debug_tui_records_turn_round_and_tool_events() {
        let logs = TempDir::new("debug_tui_logs");
        let _env = DebugEnv::set(&logs.0);

        let model = MockModel::new("m");
        model.push_tool_call("bash", serde_json::json!({"command": "echo hi"}));
        model.push_text("done");
        let mut h = spawn_app_loop_debug(Box::new(model), true, Vec::new()).await;
        h.requests.send(prompt("hi")).await.unwrap();
        let _ = collect_until_view(&mut h).await;
        shutdown(h).await;

        let files = debug_files(&logs.0);
        assert_eq!(files.len(), 1, "一个会话一个 debug 文件：{files:?}");
        let records = &files[0].1;
        let evs = record_evs(records);
        for expected in [
            "run_start",
            "turn_start",
            "round_start",
            "tool_call",
            "round_end",
            "turn_end",
            "run_end",
        ] {
            assert!(
                evs.contains(&expected),
                "缺少 `{expected}` 记录：{evs:?}"
            );
        }
        assert_eq!(
            evs.iter().filter(|ev| **ev == "turn_start").count(),
            1,
            "一个回合恰一条 `turn_start`"
        );
        assert_eq!(
            evs.iter().filter(|ev| **ev == "turn_end").count(),
            1,
            "一个回合恰一条 `turn_end`"
        );

        // 既有文本行形状不变（逐字节口径由 `round_clock` 的单测钉住）。
        let text_log = std::fs::read_to_string(logs.0.join("yushan.log")).unwrap();
        assert!(
            text_log.lines().any(|line| line.contains(" round=1 ")),
            "逐轮文本行应照旧：{text_log}"
        );
        assert!(
            text_log.lines().any(|line| line.contains(" turn=1 rounds=")),
            "回合文本行应照旧：{text_log}"
        );
    }

    /// `/new`：切到**新 debug 文件**；新文件以新 `run_start` 开头，旧文件不再收到
    /// 新回合的记录。
    ///
    /// 变异：`/new` 臂里不调 `switch_file` ⇒ 只有一个文件、旧文件含 turn=2 ⇒ 红。
    #[tokio::test]
    async fn new_session_switches_debug_file() {
        let logs = TempDir::new("debug_new_session_logs");
        let _env = DebugEnv::set(&logs.0);

        let model = MockModel::new("m");
        model.push_text("first");
        model.push_text("second");
        let mut h = spawn_app_loop_debug(Box::new(model), true, Vec::new()).await;

        h.requests.send(prompt("hi")).await.unwrap();
        let _ = collect_until_view(&mut h).await;

        h.requests.send(Request::NewSession).await.unwrap();
        let _ = collect_until_view(&mut h).await;

        h.requests.send(prompt("again")).await.unwrap();
        let _ = collect_until_view(&mut h).await;
        shutdown(h).await;

        let files = debug_files(&logs.0);
        assert_eq!(files.len(), 2, "/new 后应有新旧两个 debug 文件：{files:?}");
        let (old_path, old_records) = &files[0];
        let (new_path, new_records) = &files[1];
        assert_ne!(old_path, new_path);

        assert!(
            !old_records
                .iter()
                .any(|record| record["turn"].as_u64() == Some(2)),
            "旧文件不得再收到新会话的记录：{old_path:?}"
        );
        assert!(
            new_records.iter().any(|record| record["ev"] == "run_start"),
            "新文件应以一条 `run_start` 开头"
        );
        assert!(
            new_records
                .iter()
                .any(|record| record["turn"].as_u64() == Some(2)),
            "新文件应包含新回合的记录"
        );
    }

    /// `/resume` 同一 session：**追加同一文件**，`run_start` 的 `run` 递增。
    ///
    /// 变异：同 session 也调 `switch_file`（或 `begin_run` 不递增 run）⇒ 文件数 / run
    /// 断言红。
    #[tokio::test]
    async fn resume_same_session_appends_and_increments_run() {
        let logs = TempDir::new("debug_resume_logs");
        let _env = DebugEnv::set(&logs.0);

        let model = MockModel::new("m");
        model.push_text("hello");
        let mut h = spawn_app_loop_full(
            Box::new(model),
            true,
            Vec::new(),
            |config, _tmp| {
                config.api_base = Some("http://resume-base/v1".into());
                config.api_key = Some("sk-resume".into());
                config.set_model_factory(|_| Some(Box::new(MockModel::new("resumed"))));
            },
            |b| b,
            |_| None,
            None,
            true,
            true,
        )
        .await;

        h.requests.send(prompt("hi")).await.unwrap();
        let _ = collect_until_view(&mut h).await;
        let session = h.session_path.get().expect("持久会话应有路径");
        let id = session
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .into_owned();

        h.requests
            .send(Request::ResumeSession { target: id })
            .await
            .unwrap();
        let _ = collect_until_view(&mut h).await;
        shutdown(h).await;

        let files = debug_files(&logs.0);
        assert_eq!(files.len(), 1, "同一 session 的 `/resume` 应追加同一文件");
        let runs: Vec<u64> = files[0]
            .1
            .iter()
            .filter(|record| record["ev"] == "run_start")
            .map(|record| record["run"].as_u64().unwrap())
            .collect();
        assert_eq!(runs.len(), 2, "启动 + `/resume` 各一条 `run_start`：{runs:?}");
        assert!(runs[1] > runs[0], "`/resume` 的 run 必须递增：{runs:?}");
    }

    /// Abort：UI 投递的 `Boundary::Abort` 同时留下 `enqueued` 与 `observed` 两条记录。
    ///
    /// 变异：删掉 boundary 分支里的 `record_boundary(enqueued)` ⇒ 第一条断言红；
    /// 删掉回合末的 `Cancelled` 分支 ⇒ `observed` 缺失 ⇒ 红。
    #[tokio::test]
    async fn abort_records_enqueued_and_observed() {
        let logs = TempDir::new("debug_abort_logs");
        let _env = DebugEnv::set(&logs.0);

        let (model, mut probe) = probe_model(ProbePlan::AlwaysToolCall);
        let mut h = spawn_app_loop_debug(Box::new(model), false, Vec::new()).await;
        h.requests.send(prompt("hi")).await.unwrap();
        probe.wait_entered(1).await;
        h.boundaries.send(Boundary::Abort).await.unwrap();
        probe.let_app_task_pump().await;
        probe.release();

        let _ = collect_until_view(&mut h).await;
        shutdown(h).await;

        let files = debug_files(&logs.0);
        assert_eq!(files.len(), 1);
        let boundaries: Vec<&serde_json::Value> = files[0]
            .1
            .iter()
            .filter(|record| record["ev"] == "boundary")
            .collect();
        assert!(
            boundaries.iter().any(|record| {
                record["phase"] == "enqueued"
                    && record["kind"] == "abort"
                    && record["origin"] == "ui"
            }),
            "缺少 UI Abort 的 enqueued 记录：{boundaries:?}"
        );
        assert!(
            boundaries
                .iter()
                .any(|record| record["phase"] == "observed" && record["kind"] == "abort"),
            "缺少回合收场时的 observed 记录：{boundaries:?}"
        );
    }

    /// TUI 层的 T1：debug 关时**零新文件**（`open_enabled` 早退）。
    ///
    /// 变异：去掉 `open_enabled` 的开关判断 ⇒ 出现 `debug-*.jsonl` ⇒ 红。
    #[tokio::test]
    async fn debug_off_tui_creates_no_files() {
        let logs = TempDir::new("debug_off_tui_logs");
        let _env = DebugEnv::set(&logs.0);

        let model = MockModel::new("m");
        model.push_text("hello");
        // debug 关，但本测试仍持有 env_lock（`DebugEnv`）⇒ `env_prelocked = true`。
        let mut h = spawn_app_loop_full(
            Box::new(model),
            false,
            Vec::new(),
            |_, _| {},
            |b| b,
            |_| None,
            None,
            false,
            true,
        )
        .await;
        h.requests.send(prompt("hi")).await.unwrap();
        let _ = collect_until_view(&mut h).await;
        shutdown(h).await;

        assert!(
            debug_files(&logs.0).is_empty(),
            "debug 关时不得出现 debug 文件"
        );
    }
}
