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
//!     SetTheme{theme} => { theme = parse(theme); 落盘 state.json }  // 进程级镜像 + Output
//!     SetEffort/SetModel/Login/Logout/NewSession/Export => { 能力函数 → Output }
//!     ClearSession => { 换新会话文件（旧文件保留） → SessionCleared + Output }
//!     // Compact 是唯一先发一条的：开始行必须在压缩那次 `await` **之前**落地
//!     Compact => { opening → Output(“Compacting…”); lines = compact(..) }
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
//! **④ 每次用户输入跑一个回合，至多追加一个 harness 回合**：
//! 追加只能由**外部可观测事实**触发（`git HEAD` 前后不等，见 [`crate::post_turn`]），
//! **不能**由模型自述触发。ADR-0041 撤回的是后者（模型说「我做完了」），不是前者。
//! 追加至多一次由**调用点的结构**保证（`Request::Prompt` 臂里一个顺序 `if`，
//! 没有计数器也没有循环），不是靠配置上限。
//! 回合前的边界排空仍在 `run_turn_once` 开头 —— 它挡的是**上一个请求**残留的
//! `Abort`，与回合数无关。
//! 定时循环（`/loop`）不受本条约束：它**按时间**到点即起回合，不以外部事实为条件。

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Instant;

use tokio::sync::mpsc;
use ys_core::{ContentBlock, Message, Role, StopReason};
use ys_event::AgentEvent;
use ys_loop::{AgentInput, LoopError, RunResult};
use ys_protocol::{Boundary, Envelope, Outbound, QueueBoundarySource, Request};
use ys_runtime::{Agent, AgentPorts};
use ys_tools_basic::{BackgroundTasks, render_completion};
use ys_tui_coding::{CodingView, ThemeKind};

use crate::capabilities::{self, CompactOpening};
use crate::config::Config;
use crate::debug_log::{ArgsMode, DebugSink};
use crate::logging;
use crate::provider::{ApiModel, FetchModelsResult, ModelCache, ProviderRegistry};
use crate::state::StateStore;
use crate::status::TurnStats;
use crate::view::build_view_with_loop;
use crate::wiring::Wiring;

/// 共享 sweeper 的 tick 间隔 —— 100ms。
const BG_SWEEPER_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// 共享 sweeper 的进程级条目 —— (注册表的弱引用, sweeper 任务句柄)。
type BgSweeperEntry = (Weak<BackgroundTasks>, tokio::task::JoinHandle<()>);

/// 共享 sweeper 的**进程级**守卫 —— 每个后台注册表至多一个 sweeper 任务。
///
/// **为什么是进程级而不是每条命令一个**：设计 §4.7 的预算表要求全局 1 个
/// tokio 任务（100ms tick 调 `sweep()`）⇒ 命令再多也只有 1 个 tick 源。
/// 注册表本身是进程级的（生产路径一个进程一个注册表）⇒ 生产上就是**全局 1 个**。
///
/// **为什么按注册表分别记**：sweeper 只盯它启动时拿到的那个注册表 ⇒ 两个注册表
/// 共用一个 sweeper 就会漏扫其中一个。注册表可以有多个（测试里每个用例一份），
/// 故按注册表身份分别记。
///
/// **为什么记「当前是否活着」而不是「曾经启过」**：sweeper 任务随它所在的
/// tokio runtime 结束而消失（测试里每个用例一个 runtime）⇒ 只记「启过」会让
/// 之后的 runtime 里**永远没有** sweeper。故守卫记的是**当前是否有活着的任务**：
/// 旧的没了才启新的 —— 任何时刻每个注册表至多一个。
static BG_SWEEPERS: OnceLock<Mutex<Vec<BgSweeperEntry>>> = OnceLock::new();

/// 启动共享 sweeper —— 该注册表已有活着的就不重复启。
///
/// 100ms tick 调 [`BackgroundTasks::sweep`]：回收已退出的子进程、淘汰超出条目
/// 上限的最旧已完成条目、执行输出上限闸门。
///
/// 任务随 tokio runtime 结束而结束（**不引入新的长期信道**）；注册表被丢弃后
/// 任务自行收摊（它只持**弱引用**）。
fn ensure_background_sweeper(tasks: Arc<BackgroundTasks>) {
    let slot = BG_SWEEPERS.get_or_init(|| Mutex::new(Vec::new()));
    let mut guard = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // 注册表没了 ⇒ 它的 sweeper 也该收摊 ⇒ 条目一并清掉（否则这张表只增不减）。
    guard.retain(|(weak, _)| weak.strong_count() > 0);
    let key = Arc::as_ptr(&tasks) as usize;
    let alive = guard
        .iter()
        .any(|(weak, handle)| Weak::as_ptr(weak) as usize == key && !handle.is_finished());
    if alive {
        return; // 该注册表已有活着的 sweeper
    }
    let weak = Arc::downgrade(&tasks);
    let task_weak = weak.clone();
    let handle = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(BG_SWEEPER_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let Some(tasks) = task_weak.upgrade() else {
                return; // 注册表没了 ⇒ 收摊
            };
            tasks.sweep();
        }
    });
    guard.push((weak, handle));
}

/// 当前会话 id（后台注册表的会话键）—— `None` = 本接线器不带后台能力。
fn background_session_id(wiring: &Wiring) -> Option<String> {
    wiring.backgrounds().map(|context| context.session_id())
}

/// 把「可选的唤醒源」变成一条 `select!` 分支：无源 ⇒ `None`。
///
/// 用 `Some(..) = …` 模式形式接住结果：模式不匹配时 tokio **禁用**该分支（本次
/// `select!` 不再轮询它）⇒ 未挂该能力时既不会忙转，也不会被它抢先返回。
async fn optional_wake<F: std::future::Future>(source: Option<F>) -> Option<F::Output> {
    match source {
        Some(source) => Some(source.await),
        None => None,
    }
}

/// 杀掉某会话的全部后台命令进程组（**按会话过滤**）。
///
/// 接线器不带后台上下文时**静默跳过** —— 清理点不得因为「没有后台能力」而 panic。
fn kill_background_session(wiring: &Wiring, session: &str) {
    if let Some(context) = wiring.backgrounds() {
        context.tasks().kill_all(session);
    }
}

/// 丢弃一个会话的后台产物：杀进程组 + 删落盘目录。
///
/// `/new` 用它（旧会话整体作废）；`/resume` 只杀进程组、**不删目录** ——
/// 目标会话的产物还要留着。
fn discard_session_backgrounds(wiring: &Wiring, session: &str) {
    if let Some(context) = wiring.backgrounds() {
        context.tasks().kill_all(session);
        // 目录删不掉（权限 / 并发占用）**不影响控制流**：残留只占磁盘。
        let _ = context.tasks().remove_session_dir(session);
    }
}

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
    /// 有子 agent 的结果落地到跨回合投递表（ADR-0065）。
    ///
    /// 无此分支时，一轮「回合已结束、子 agent 稍后才回来」的结果会**静默滞留**到用户
    /// 下次敲字（表没有回合外读者）。结果是一种输入 ⇒ 这里**自动起一轮**把它交给模型
    /// 处理，与 [`Wake::BackgroundReturned`] 同构。
    ///
    /// 起回合前先 [`PendingResults::has_pending`] 复核：唤醒只说「可能有」（permit 还
    /// 可能跨回合残留），空则什么都不做。
    SubagentReturned,
    /// 有后台命令**跑完了**（进了终态、结果留在投递表里）。
    ///
    /// 这里**自动起一轮**把结果交给模型处理：后台命令的返回是一种输入，而命令本身
    /// 就是工作内容 —— 跑完就该继续，不该搁置到用户下次开口。起回合前先
    /// [`BackgroundTasks::has_pending`](ys_tools_basic::BackgroundTasks::has_pending)
    /// 复核：唤醒只说「可能有」（清扫是进程级、会话级过滤在这一侧），空则什么都不做。
    ///
    /// 与 [`Wake::SubagentReturned`] 同构（两者都是「后台工作的结果回来了 ⇒ 起一轮交给
    /// 模型」），差别只在复核判据：本条按会话过滤（清扫是进程级的），子 agent 的结果
    /// 不分会话。
    ///
    /// **不会热循环**：`sweep` 的 `notify` 只在**新终态**时发（`newly_terminal > 0`），
    /// 而本回合把 `Pending` 一次取完（`take_completions`） ⇒ 醒来时表里已空。
    /// 取出的条目先落 `InFlight`，投递失败才回 `Pending`，而那要等下一次新终态唤醒。
    BackgroundReturned,
    /// 定时循环到点（`/loop` 设的那个）。
    ///
    /// **醒来即起一个回合。** 语义是「到点做一次这件事」：不探外部状态、不设前置
    /// 判据 —— 要不要干活由用户在提示词里说清，由模型自己查、自己判断。
    /// 停的条件只有显式路径（`/loop off`、`/new`、`/clear`、`/resume` 换会话）。
    ///
    /// 与 [`Wake::BackgroundReturned`] 同构（都是「外部事件 ⇒ 起一轮」），
    /// 差别只在激活源是时间而非某个组件的 `notify`。
    TimerFired,
}

/// 定时循环的**运行期状态**（一个 session 至多一个）。
///
/// 存在 `app_loop::run` 的主循环局部变量里，**不进 `Wiring`** ——
/// 它不属于任何组件，生命周期就是本 `run` 调用。
///
/// **单条语义**：一个 session 一个循环，`/loop` 再敲一次即覆盖。多条要处理
/// 「同时到点怎么办」（串行 / 合并 / 排队），需求反而比看上去少 ——
/// 那几件事本来就能写进同一条提示词。
/// 甲→乙是加法：把 `next_at` 换成堆，登记与动作两层一行不动。
struct LoopState {
    /// 下次唤醒时刻。**每次醒来都先推远**（无论是否起回合）——
    /// 否则 `select!` 立刻再次就绪，循环忙转不花钱但占满 CPU。
    next_at: Instant,
    /// 间隔（`next_at` 由它推进）。
    interval: std::time::Duration,
    /// 给模型的提示词（原样作为 tick 消息正文）。
    prompt: String,
}

impl LoopState {
    /// 推到下一个周期，返回新的唤醒时刻。
    fn advance(&mut self) -> Instant {
        self.next_at = Instant::now() + self.interval;
        self.next_at
    }

    /// 间隔的显示串（`5m` / `90s` / `2h`）：状态行用它，渲染层不再算。
    fn interval_label(&self) -> String {
        let secs = self.interval.as_secs();
        if secs.is_multiple_of(3600) {
            format!("{}h", secs / 3600)
        } else if secs.is_multiple_of(60) {
            format!("{}m", secs / 60)
        } else {
            format!("{secs}s")
        }
    }

    /// 视图投影：空闲时带「还有多久」，正在跑回合时 `next_at` 为 `None`。
    ///
    /// 用**相对时长**而不是钟点：“14 分后”与消息时区无关、永远正确；
    /// 而“14:32”需要本地时区偏移，本 crate 拿不到（`ys-tui-coding::clock` 是
    /// `pub(crate)`）—— 冒充一个 UTC 钟点会让用户按错的时间等待。
    fn view(&self, turning: bool) -> ys_tui_coding::LoopView {
        ys_tui_coding::LoopView {
            interval: self.interval_label(),
            next_label: (!turning)
                .then(|| relative_label(self.next_at.saturating_duration_since(Instant::now()))),
        }
    }
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

    // **共享 sweeper**：全局 1 个 tokio 任务，100ms tick 调 `sweep()`
    // （回收已退出的子进程、淘汰超出条目上限的最旧已完成条目、执行输出上限闸门）。
    // 本接线器不带后台能力时**不启**（无事可扫）。
    if let Some(context) = wiring.backgrounds() {
        ensure_background_sweeper(context.tasks().clone());
    }

    // `/thinking` 显示开关的**进程级缓存**：启动时由 meta 初始化，`Request::SetThinking`
    // 单向更新，回合末落 meta。UI 的 `App.thinking_visible` 才是运行期真值，本缓存只
    // 作持久化镜像；**`/new` 不重置它**（展示偏好随进程，设计 §5.6）。
    let mut thinking_visible = wiring.meta().map(|m| m.thinking_visible).unwrap_or(false);

    // 主题的**进程级镜像**：启动时由 state.json 初始化，`Request::SetTheme` 单向更新
    // 并落盘；`build_view` 从它取值（不在 `build_view` 里读盘 —— 那是每请求一次的 IO）。
    //
    // 与 `thinking_visible` 同类（会话级偏好、UI 本地即时生效、本处只作持久化镜像），
    // 但**载体不同**：主题是**全局偏好**，**不进 session meta**（`thinking_visible`
    // 的载体是 `SessionMeta`）。`/new` 不重置它、`/resume` 换会话也不改它 ——
    // 别为了「对齐」把它塞进 meta。
    let mut theme: ThemeKind = state_store
        .load()
        .theme
        .as_deref()
        .and_then(|s| ThemeKind::from_str(s).ok())
        .unwrap_or_default();

    // 定时循环（`/loop`）。`None` = 没有循环在跑 —— 默认就是没有。
    //
    // **不落盘、不随 `--resume` 自动续跑**：恢复后旧循环**不继承**，否则用户
    // 以为自己只是接着读历史，实际上回来就持续烧钱。UI 侧会给一次性提示。
    let mut loop_state: Option<LoopState> = None;

    // 上一次注入清单时的 `revision`（`None` = 本进程还没注入过）。
    //
    // **必须活过 `Request::Prompt` 臂**：臂每进一次都是新的局部作用域，而这个量描述的是
    // 「会话历史里那份注入对应哪个版本」，跨回合才有意义。
    // `/new` / `/resume` 换会话时不重置 —— 新历史里找不到标记，注入判据的自愈分支会兜住；
    // 重置它只是同一结果。
    let mut last_injected_rev: Option<u64> = None;

    // 清单真值的句柄（与 `todo` 工具持的是**同一个 `Arc`**）。
    // `/new` / `/resume` 改的是它内部的指向（`retarget`），这里不需要重新取。
    let todo_store = factory.todo();

    loop {
        // 等一条 UI 请求；后台模型目录若尚未返回，则与之赛跑。目录**一次性**到达，
        // 收到后把 `models_rx` 置 `None`，此后主循环只等 `Request`。
        //
        // 目录到手 ⇒ 并入 registry、刷新一次视图：`/model` 候选此刻才拿到这两家的
        // 模型名（渲染路径依旧一次 HTTP 都不发，写与读同在这个线程）。
        let wake = {
            // 「子 agent 结果落地」与「后台命令跑完」这两条分支只在挂了对应能力时存在。
            // 一律用 `Some(..) = …` 模式形式：源为 `None` 时该分支被 tokio **禁用**，
            // 不会忙转。
            let pending = subagent.as_ref().map(|s| s.pending_results().notified());
            let background = wiring
                .backgrounds()
                .map(|context| context.tasks().notified());
            // 定时唤醒：`None`（没设循环）时该分支被 tokio **禁用**，不忙转。
            // `sleep_until` 吸收 `Instant`，到点即就绪。
            let timer = loop_state
                .as_ref()
                .map(|state| tokio::time::sleep_until(state.next_at.into()));
            tokio::select! {
                req = request_rx.recv() => Wake::Request(req),
                Some(cache) = optional_wake(models_rx.as_mut().map(|rx| rx.recv())) => {
                    Wake::Models(cache)
                }
                Some(()) = optional_wake(pending) => Wake::SubagentReturned,
                Some(()) = optional_wake(background) => Wake::BackgroundReturned,
                Some(()) = optional_wake(timer) => Wake::TimerFired,
            }
        };
        let req = match wake {
            Wake::Request(Some(req)) => req,
            Wake::Request(None) => {
                // UI 侧 drop 了发送端 → 正常退出前排空 debug 队列并写 `run_end`。
                flush_debug(&mut debug, &stats, run_rounds, 0);
                return Ok(());
            }
            Wake::SubagentReturned => {
                // 复核必须留：唤醒只说「可能有」（permit 可能跨回合残留），也可能结果
                // 已被某个回合的补投取走 ⇒ 空则什么都不做。`has_pending` **只读不取**：
                // 真正取走结果的是本回合开头的补投（`take_pending_results`），双取会重复送达。
                let has_pending = subagent
                    .as_ref()
                    .is_some_and(|s| s.pending_results().has_pending());
                if !has_pending {
                    continue;
                }
                // 占位输入**不是**用户说的话 ⇒ `Role::Harness`。真正的内容是随后经
                // `Boundary::Deferred` 注入的那几条子 agent 结果。
                let message = subagent_placeholder();
                // 返回值（`TurnOutcome`）**有意丢弃**：自动回合之后不再追问 ——
                // 追问只在用户发言后发生（追问的判据靠「HEAD 在用户回合里变了」）。
                let _ = run_turn_chain(
                    &mut agent,
                    &mut wiring,
                    &config,
                    &mut turns,
                    &mut stats,
                    thinking_visible,
                    &mut run_rounds,
                    &out_tx,
                    &mut sink_rx,
                    &mut boundary_rx,
                    subagent.as_ref(),
                    debug.as_mut(),
                    &debug_session_id,
                    &worktree,
                    message,
                    // 占位消息不是用户说的话 ⇒ 不贡献标题素材。
                    None,
                )
                .await?;
                // 回合收场后刷新视图（本臂 `continue`，走不到循环尾部那次刷新）——
                // 否则状态行会停在自动回合**之前**的读数上。
                let view = build_view_with_loop(
                    &config,
                    &agent,
                    &wiring,
                    &config.registry,
                    &state_store,
                    &stats,
                    session_started,
                    theme,
                    loop_state.as_ref().map(|s| s.view(false)),
                );
                send_or_flush(
                    &out_tx,
                    Outbound::View(view),
                    &mut debug,
                    &stats,
                    run_rounds,
                )
                .await?;
                continue;
            }
            Wake::BackgroundReturned => {
                // 会话 id 在**唤醒时**从共享槽读（`/new` / `/resume` 会换槽）⇒ 只看
                // 本会话的完成项。复核必须留：唤醒只说「可能有」（清扫是进程级的），
                // 也可能来自别的会话，或结果已被某个回合的补投取走
                // （`InFlight` / `Delivered` 都不算 `Pending`）⇒ 空则什么都不做。
                let has_pending = wiring.backgrounds().is_some_and(|context| {
                    should_start_background_turn(context.tasks(), &context.session_id())
                });
                if !has_pending {
                    continue;
                }
                // 占位输入**不是**用户说的话 ⇒ `Role::Harness`：`BasicLoop` 照它 append
                // 进会话（与任何回合的输入同一条路径），TUI 据 `Role` 画 System 行。
                // 真正的内容是随后经 `Boundary::Deferred` 注入的那几条结果。
                let message = background_placeholder();
                // 返回值（`TurnOutcome`）**有意丢弃**，理由同 `Wake::SubagentReturned`。
                let _ = run_turn_chain(
                    &mut agent,
                    &mut wiring,
                    &config,
                    &mut turns,
                    &mut stats,
                    thinking_visible,
                    &mut run_rounds,
                    &out_tx,
                    &mut sink_rx,
                    &mut boundary_rx,
                    subagent.as_ref(),
                    debug.as_mut(),
                    &debug_session_id,
                    &worktree,
                    message,
                    // 占位消息不是用户说的话 ⇒ 不贡献标题素材。
                    None,
                )
                .await?;
                // 回合收场的快照刷新与其它请求同规（本臂 `continue`，走不到循环尾部
                // 的那次刷新）—— 否则状态行会停在自动回合**之前**的读数上。
                let view = build_view_with_loop(
                    &config,
                    &agent,
                    &wiring,
                    &config.registry,
                    &state_store,
                    &stats,
                    session_started,
                    theme,
                    loop_state.as_ref().map(|s| s.view(false)),
                );
                send_or_flush(
                    &out_tx,
                    Outbound::View(view),
                    &mut debug,
                    &stats,
                    run_rounds,
                )
                .await?;
                continue;
            }
            Wake::TimerFired => {
                // **先推远下次唤醒**：不推远就会立刻再次就绪 ⇒ 忙转。取 `state` 时
                // 顺手推远，此后本回合无论成与败，下个周期都已排好。
                let prompt = match loop_state.as_mut() {
                    Some(state) => {
                        state.advance();
                        state.prompt.clone()
                    }
                    // 定时器已在其它分支被清掉（`/loop off` 与本分支的竞争）⇒ 什么都不做。
                    None => continue,
                };

                // 标记「正在跑」：状态行要能看到 `正在跑` 而不是一个过期的「几分钟后」。
                let turning_view = loop_state.as_ref().map(|s| s.view(true));
                send_or_flush(
                    &out_tx,
                    Outbound::View(build_view_with_loop(
                        &config,
                        &agent,
                        &wiring,
                        &config.registry,
                        &state_store,
                        &stats,
                        session_started,
                        theme,
                        turning_view.clone(),
                    )),
                    &mut debug,
                    &stats,
                    run_rounds,
                )
                .await?;

                let _ = run_turn_chain(
                    &mut agent,
                    &mut wiring,
                    &config,
                    &mut turns,
                    &mut stats,
                    thinking_visible,
                    &mut run_rounds,
                    &out_tx,
                    &mut sink_rx,
                    &mut boundary_rx,
                    subagent.as_ref(),
                    debug.as_mut(),
                    &debug_session_id,
                    &worktree,
                    loop_placeholder(&prompt),
                    // 定时消息不是用户说的话 ⇒ 不贡献标题素材。
                    None,
                )
                .await;

                // 回合收场后刷新：本臂 `continue`，走不到循环尾部那次刷新。
                let idle_view = loop_state.as_ref().map(|s| s.view(false));
                let view = build_view_with_loop(
                    &config,
                    &agent,
                    &wiring,
                    &config.registry,
                    &state_store,
                    &stats,
                    session_started,
                    theme,
                    idle_view,
                );
                send_or_flush(
                    &out_tx,
                    Outbound::View(view),
                    &mut debug,
                    &stats,
                    run_rounds,
                )
                .await?;
                continue;
            }
            Wake::Models(cache) => {
                models_rx = None; // 目录只投一次；此后不再参与 `select!`
                if let Some(cache) = cache {
                    config.registry.apply_model_cache(cache);
                    let view = build_view_with_loop(
                        &config,
                        &agent,
                        &wiring,
                        &config.registry,
                        &state_store,
                        &stats,
                        session_started,
                        theme,
                        loop_state.as_ref().map(|s| s.view(false)),
                    );
                    send_or_flush(
                        &out_tx,
                        Outbound::View(view),
                        &mut debug,
                        &stats,
                        run_rounds,
                    )
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

                // **回合边界 · 清单注入**（`docs/arch/long-horizon-todo/design.md` §7.3）：
                // 在**回合之前**把当前清单推进上下文，使本回合的每一轮都看得见它。
                //
                // 角色是 `Role::Harness` 而非 `Role::User`：用户没说过这些话（出站时两者
                // 都映射成 user，模型可见效果相同，但 TUI 不会把它画成用户发言）。
                // **直接 append、不发事件** ⇒ UI 不显示 —— 与 `Boundary::Steer` 的区别就在此。
                //
                // 判据是纯函数（见 [`crate::todo_inject`]）：状态变了 **或** 历史里已找不到
                // 标记 ⇒ 注入。后一条让压缩（含不发事件的手敲 `/compact`）之后自动补回。
                if crate::todo_inject::should_inject(
                    &todo_store,
                    last_injected_rev,
                    wiring.session_messages(),
                ) {
                    let text = crate::todo_inject::injection_text(&todo_store);
                    wiring
                        .append_message(Message {
                            role: Role::Harness,
                            content: vec![ContentBlock::Text { text }],
                        })
                        .await?;
                    last_injected_rev = Some(todo_store.revision());
                }

                // 回合**前**记 HEAD。
                //
                // 取点用 `config.cwd`（回合真正的工作目录），**不用** `worktree.get()`：
                // 后者在「共享工作区」下是 `None`（`main.rs` 用 `.zip()` 组绑定，
                // 未 attach 树时两半都缺）—— 那是绝大多数启动形态，用它会静默地
                // 让本功能永不生效。`config.cwd` 在 `/resume` 切工作区时已被
                // `apply_resumed_workspace` 刷新（`:1143`）⇒ 它才是「本回合在哪跑」。
                //
                // 开关关时**不 spawn 两次 git**：判据默认关，而那两次是每回合
                // 实打实的 fork+exec。先把开关读完，关就不取。
                let enabled = config.follow_up_on_commit();
                let workspace = enabled.then(|| config.cwd.clone());
                let head_before = crate::post_turn::git_head(workspace.as_deref());

                let outcome = run_turn_chain(
                    &mut agent,
                    &mut wiring,
                    &config,
                    &mut turns,
                    &mut stats,
                    thinking_visible,
                    &mut run_rounds,
                    &out_tx,
                    &mut sink_rx,
                    &mut boundary_rx,
                    subagent.as_ref(),
                    debug.as_mut(),
                    &debug_session_id,
                    &worktree,
                    message,
                    raw_user_text,
                )
                .await?;

                // ── 跨回合推进（`docs/arch/cross-turn-advance/design.md` §5.8）──
                //
                // 跑完首个回合后进这个循环：每轮问一次「要不要再来一轮」，
                // 判据由 [`crate::turn_advance::advance`] 给出。
                //
                // **一期的 git 追问行为原样保留**：没有清单时 `decide` 返回
                // `Stop(NoPlan)`，`advance` 退回 `should_follow_up` 的等价判据
                // （HEAD 前后不等 ∧ 开关开 ∧ 正常收场），结果与接线前完全一致。
                //
                // 每个追问回合**之前**重新注入清单（与首个回合同一个注入点）：
                // 追问回合的模型必须看得见本回合结束时的清单状态。
                let mut advance_state = crate::turn_advance::AdvanceState::new();
                let mut last_outcome = outcome;
                // 每个追问回合的**起点**：判官事实里的「耗时」指这一轮花了多久，
                // 不是整个请求的总时长 —— 后者会把前几轮的成本算到后一轮头上。
                let mut advance_turn_started = std::time::Instant::now();
                loop {
                    // ── 判官（二档判据）──
                    //
                    // 只在**本回合写过清单**时调用（判据在 `judge_failure_delta` 里）。
                    // 调用失败 / 解析不可靠 ⇒ `judge_failures += 1`，可用结论 ⇒ 归零。
                    let head_after = crate::post_turn::git_head(workspace.as_deref());
                    let model = wiring.model_arc();
                    let delta = crate::turn_advance::judge_failure_delta(
                        model.as_deref(),
                        wiring.session_messages(),
                        &todo_store,
                        head_before.as_deref(),
                        &head_after,
                        &last_outcome,
                        Some(advance_turn_started.elapsed()),
                    )
                    .await;
                    if delta == 0 {
                        advance_state.judge_failures = 0;
                    } else {
                        advance_state.judge_failures =
                            advance_state.judge_failures.saturating_add(delta);
                    }

                    let step = crate::turn_advance::advance(
                        &todo_store,
                        &mut advance_state,
                        &last_outcome,
                        enabled,
                        head_before.as_deref(),
                        head_after,
                    );
                    let crate::turn_advance::Advance::Push = step else {
                        break;
                    };

                    // 追问回合**之前**再注入一次清单（与首个回合同一个注入点、同一判据）。
                    if crate::todo_inject::should_inject(
                        &todo_store,
                        last_injected_rev,
                        wiring.session_messages(),
                    ) {
                        let text = crate::todo_inject::injection_text(&todo_store);
                        wiring
                            .append_message(Message {
                                role: Role::Harness,
                                content: vec![ContentBlock::Text { text }],
                            })
                            .await?;
                        last_injected_rev = Some(todo_store.revision());
                    }

                    advance_turn_started = std::time::Instant::now();
                    last_outcome = run_turn_chain(
                        &mut agent,
                        &mut wiring,
                        &config,
                        &mut turns,
                        &mut stats,
                        thinking_visible,
                        &mut run_rounds,
                        &out_tx,
                        &mut sink_rx,
                        &mut boundary_rx,
                        subagent.as_ref(),
                        debug.as_mut(),
                        &debug_session_id,
                        &worktree,
                        crate::post_turn::harness_input(),
                        // 占位输入没有用户原文 ⇒ 标题素材传 `None`（不能拿它当标题）。
                        None,
                    )
                    .await?;
                    advance_state.used = advance_state.used.saturating_add(1);
                }
            }

            Request::Loop {
                interval_secs,
                prompt,
                run_now,
            } => {
                // **只登记，不起回合**：本臂不调 `run_turn_chain`、不刷视图，跑回合的
                // 唯一入口是 `Wake::TimerFired`（它自带「正在跑」视图与 `advance()` 防忙转）。
                // 但 `run_now` 会把首次唤醒时刻设成**现在** ⇒ 下一次 `select!` 立即就绪
                // ⇒ 首个回合当场起来；`advance()` 随即把下一次推到 `now + interval`，
                // 「跑一次、然后按间隔继续」自动成立。缺省 `false` 则等满一个间隔
                // —— 用户敲 `/loop 5m 检查 CI` 时人在屏前，没有必要立即花一次模型调用；
                // 而“立即跑一次”会让「设完就走」的人等两回，故它需要显式 `/loop now`。
                //
                // **单条语义**：再敲一次即覆盖（不叠加、不排队）。
                //
                // **无条件登记**：不预检工作区形态 —— 循环只认时间，任何目录都能跑。
                let interval = std::time::Duration::from_secs(interval_secs);
                let first = if run_now {
                    Instant::now()
                } else {
                    Instant::now() + interval
                };
                loop_state = Some(LoopState {
                    next_at: first,
                    interval,
                    prompt,
                });
                // **不在这里刷视图**：本臂末尾会落到循环尾部的统一刷新，
                // 那里已用 `build_view_with_loop` 带上 `loop_state`。多刷一次会让
                // 每个 `/loop` 产生两个 View —— UI 白重绘一帧，测试也会把第一条
                // 命令的第二个 View 误认成第二条命令的结果。
            }

            Request::LoopOff => {
                // **已在跑的回合不中断**：取消只影响“下一次唤醒”，不影响当前回合。
                // 硬规则 —— 用户敲 `/loop off` 通常是发现它跑得不对，而不是要丢掉
                // 手头这一轮已经花了钱的结果。
                //
                // 没在跑就不发文案：`/loop off` 幂等，静默成功（与 `SetThinking` /
                // `SetTheme` 同纪律：无变化不产用户可见输出）。**不得 `push("")`**——
                // 全空串的 `Vec` 是 non-empty，会发一条 `Outbound::Output("")`，
                // 在 transcript 里落一条空白 System 行。
                if loop_state.take().is_some() {
                    lines.push("Loop stopped.".to_string());
                }
                // 视图交给循环尾部的统一刷新（它按 `loop_state` 现算，此刻已是 `None`）。
            }

            Request::SetThinking { visible } => {
                // 单向更新进程级镜像；UI 侧已本地即时生效。无输出。
                thinking_visible = visible;
            }

            Request::SetTheme { theme: name } => {
                // 协议层只承载 kebab-case 名、不解析（`ThemeKind` 归 `ys-tui-coding`），
                // 校验与归一就在这一层：非法名只警告、**不落盘**。
                //
                // UI 侧已经本地即时生效（`/theme <name>` / 浮层确认），本臂只负责
                // 更新镜像 + 持久化 —— 主题是全局偏好，不进 session meta。
                match ThemeKind::from_str(&name) {
                    Ok(kind) => {
                        theme = kind;
                        // 读现有 state 再改自己的字段：直接构造字面量会清掉 provider/model。
                        let mut st = state_store.load();
                        st.theme = Some(kind.name().to_string());
                        if let Err(e) = state_store.save(&st) {
                            lines.push(format!("Warning: Could not persist theme: {e}"));
                        }
                        // **不回显**：UI 侧（`/theme <name>` 经 `apply_local`、浮层确认经
                        // `run.rs`）已经写了一条 `theme: <name>`。与 `SetThinking` 同一纪律 ——
                        // 单向持久化同步不产生用户可见输出，否则每条命令打两遍。
                    }
                    Err(e) => lines.push(format!("Warning: {e}")),
                }
            }

            Request::SetEffort { effort } => {
                // 设置照常保存 + 重建 model；不支持的 provider 由能力函数给出 warning。
                let before = model_snapshot(&config);
                lines = capabilities::set_effort(&mut config, &mut wiring, effort);
                record_model_change(&mut debug, &debug_session_id, &before, &config, "effort");
            }

            Request::SetModel { model, provider } => {
                let before = model_snapshot(&config);
                lines = capabilities::set_model(
                    &mut config,
                    &mut wiring,
                    &state_store,
                    &model,
                    provider.as_deref(),
                );
                record_model_change(&mut debug, &debug_session_id, &before, &config, "model");
            }

            Request::Login {
                provider,
                api_key,
                api_base,
            } => {
                // 起始模型只能从目录来：先按 `/login` 的同一规则定出 base，再把该
                // provider 的目录交给 `capabilities::login` 定模型。
                //
                // 目录的**第一手来源是端点本身**（`fetch_models`，自带 5s 超时）：这是用户
                // 显式动作 ⇒ 这次网络值得等；拉取期间 Esc 可取消（命令不过 `BasicLoop`，
                // 没人读 `Boundary`，不在这里 `select!` 一个取消口就只剩 Ctrl-C 关进程）。
                //
                // 拉不到（超时 / 401 / 空目录 / 该端点无此接口，如编码套餐端点实测 404）时
                // **回落到内置种子**（[`ProviderRegistry::seed_catalog`]）—— 种子是模型名的
                // 兜底来源，不是主来源。
                //
                // 两条路径的收尾一致：目录进 registry 缓存，`/model` 候选与窗口查询立刻可用
                // （种子在构造时已预热一份，但用户可能给了一个**别的** base，那条键要靠这里补上）。
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
                                lines = vec!["Login cancelled. Nothing was changed.".to_string()];
                            }
                            Some(fetched) => {
                                let mut catalog = match fetched {
                                    FetchModelsResult::Success(models) if !models.is_empty() => {
                                        // **先过滤再选起始模型**：目录里可能有本仓用不了的
                                        // 模型（实测 MiMo 把语音模型排在最前，而它们在
                                        // chat 端点上必然 400）⇒ 不过滤的话 `/login`
                                        // 会登到一个用不了的模型上。
                                        Some(config.registry.filter_catalog(&base, models))
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
                                // 过滤后可能**一条不剩**（目录里全是用不了的模型）——
                                // 那等同于「拉不到目录」⇒ 走种子回落。
                                if catalog.as_ref().is_some_and(|c| c.is_empty()) {
                                    catalog = None;
                                }
                                // 目录拉不到时**回落到内置种子**：编码套餐端点的
                                // `/v1/models` 实测 404，而种子在那里是唯一可用的模型名来源。
                                // 种子已经在 `model_cache` 里（构造时预热），故不需要再写一次。
                                if catalog.is_none() {
                                    catalog = config.registry.seed_catalog(&provider).map(|seed| {
                                        seed.iter()
                                            .map(|(id, window)| ApiModel {
                                                id: (*id).to_string(),
                                                owned_by: None,
                                                name: None,
                                                context_window: Some(*window),
                                            })
                                            .collect()
                                    });
                                }
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
                // **清理点（`/new`）**：换会话前先记住**旧**会话 id ——
                // 会话槽在 `new_session` 内被切到新 id，而注册表条目是按**启动时**
                // 的槽值打标签的 ⇒ 拿新 id 去清理会**打空**。
                let old_session = background_session_id(&wiring);
                // 旧会话整体作废 ⇒ 杀它的后台命令进程组 + 删它的落盘目录。
                // 落在换会话**之前**：此刻槽里还是旧 id。
                if let Some(session) = old_session.as_deref() {
                    discard_session_backgrounds(&wiring, session);
                }
                lines = capabilities::new_session(&mut wiring).await;
                // **定时循环随会话作废**：循环归 session（设计 §4.5）—— 新会话没有
                // 定时器，旧的那条便无主语。不静默继承：用户敲 `/new` 时想的是
                // 「重新开始」，而一个每秒烧钱的旧定时器显然不属于这个预期。
                loop_state = None;
                // 新会话 = 新的会话起点（状态行的时长据此重置）。
                session_started = Instant::now();
                // 换会话也换了文件 —— 刷新给 `main` 的退出提示快照。
                session_path.record(wiring.session_path());
                // 清单真值跟随会话文件：`/new` 后清单从空开始，旧会话的清单留在旧文件里。
                // 只换内部路径，`todo` 工具持有的 `Arc` 不变 ⇒ 不需要重建工具表。
                factory.todo().retarget(wiring.session_path());
                // `/new` **切新 debug 文件**（旧文件自然结束），并开一个新的 run。
                if let Some(sink) = debug.as_mut() {
                    if let Some(path) = sink.path_for_session(wiring.session_path()) {
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

            Request::ClearSession => {
                // **清理点（`/clear`）**：与 `/new` 完全一致 —— 旧会话整体作废，
                // 进程组要杀、落盘目录要删（对清理后的新工作区无意义），且注册表条目
                // 按**启动时**的槽值打标签 ⇒ 必须在换会话**之前**取旧 id。
                let old_session = background_session_id(&wiring);
                if let Some(session) = old_session.as_deref() {
                    discard_session_backgrounds(&wiring, session);
                }
                let (out_lines, ok) = capabilities::clear_session(&mut wiring).await;
                lines = out_lines;
                // 与 `/new` 同理：循环归 session，清理即作废。
                loop_state = None;
                // 新会话 = 新的会话起点（状态行的时长据此重置）。
                session_started = Instant::now();
                // 换会话也换了文件 —— 刷新给 `main` 的退出提示快照。
                session_path.record(wiring.session_path());
                // `/clear` 同理：清理后的会话清单从空开始。
                factory.todo().retarget(wiring.session_path());
                // 与 `/new` 一样切新 debug 文件（旧文件自然结束）并开一个新的 run。
                if let Some(sink) = debug.as_mut() {
                    if let Some(path) = sink.path_for_session(wiring.session_path()) {
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
                // **只在成功时清屏**：失败时清屏会出现「屏空了、历史其实还在」。
                // 下发顺序由函数末尾的 `session_event` 保证排在 `Output` 之前（I2）——
                // 反了的话清屏会把这条提示吃掉。
                if ok {
                    session_event = Some(Outbound::SessionCleared);
                }
            }

            Request::Compact => {
                // 窗口与策略都取自 `config` —— 与 agent 每回合用的那份同源
                // （`/compact` 是用户显式触发的压缩，不等自动触发阈值）。
                let policy = config.compact_policy();
                // 固定开销取自 **agent 自己持有的** `limits` ⇒ 与自动压缩**同一口径**
                // （`/compact` 若用自己的配置值，两条路径报的数字会不同口径）。
                let fixed = agent.fixed_prompt_tokens();
                // 活对话的 system（同理取自 agent）⇒ 摘要请求复用前缀、命中 KV cache。
                let live_system = agent.system_prompt().map(str::to_string);
                let window = config.effective_context_window();

                // **开始提示必须先于这次 `await` 落地**：摘要是一次独立模型调用
                // （实测数十秒），`capabilities::compact` 一进去就 await、中途没有任何
                // 出站口。攒到 `lines` 里与收场行一起发，就等于在压缩全程把屏幕冻住。
                //
                // 压不了的两种情形（无 model / 无事可压）**不得**先打「正在压缩」：
                // 判定与文案全在 `compact_opening` 里，与 `compact` 用同一份 `plan()`。
                // （第三种「摘要失败」要到模型调用回来才知道，只能由收场行报。）
                match capabilities::compact_opening(&wiring, &policy, window, fixed) {
                    CompactOpening::Blocked(blocked) => lines = blocked,
                    CompactOpening::Start(opening) => {
                        send_or_flush(
                            &out_tx,
                            Outbound::Output(opening),
                            &mut debug,
                            &stats,
                            run_rounds,
                        )
                        .await?;
                        lines = capabilities::compact(
                            &mut wiring,
                            &policy,
                            window,
                            fixed,
                            live_system.as_deref(),
                        )
                        .await;
                    }
                }
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
                // **清理点（`/resume`）**：记住**旧**会话 id —— 恢复成功后槽里
                // 就是新 id 了，旧会话的命令要按**旧 id** 才能命中。
                let old_session = background_session_id(&wiring);
                let (out_lines, resumed) =
                    capabilities::resume_session(&mut config, &mut wiring, &state_store, &target)
                        .await;
                lines = out_lines;
                if let Some(r) = resumed {
                    // **`--resume` 不自动续跑**：`loop_state` 从不落盘，故恢复后恒为
                    // `None`。本臂是「运行中换会话」：旧循环属于**旧** session，
                    // 作废。静默不续跑是对的（用户发现自己回来就持续烧钱更糟），
                    // 但**不能静默停掉一个则设的循环**。
                    //
                    // **同路径不是「换会话」**：批到同一文件时只开新 run，
                    // 工作区未变、循环依旧有效 ⇒ **保留**。否则用户只是「重开了
                    // 当前会话」就丢掉刚设的定时器，且文案（session changed）是假的。
                    let switched = wiring.session_path().map(Path::to_path_buf) != path_before;
                    if switched && loop_state.take().is_some() {
                        lines.push(
                            "Loop stopped (session changed). Use /loop to restart.".to_string(),
                        );
                    }
                    // 目标会话与当前会话**不同** ⇒ 旧会话的后台命令对新工作区
                    // 无意义 ⇒ 杀掉（按会话过滤）。同一会话（追加同一文件）
                    // ⇒ 不动 —— 否则会杀掉本会话自己的命令。
                    if let Some(old) = old_session.as_deref()
                        && background_session_id(&wiring).as_deref() != Some(old)
                    {
                        kill_background_session(&wiring, old);
                    }
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
                    // 清单真值跟随目标会话文件（`/resume` 把清单也一起恢复）。
                    factory.todo().retarget(wiring.session_path());
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
                            && let Some(path) = sink.path_for_session(path_after.as_deref())
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
        //
        // **必须走 `build_view_with_loop`**：这是**每个请求**的唯一刷新点，
        // 也是回合结束后状态行回到「空闲」的那一次。用裸 `build_view` 会把
        // `loop_state` 填回 `None` —— `/loop` 刚设好就被自己下一帧抹掉。
        let view = build_view_with_loop(
            &config,
            &agent,
            &wiring,
            &config.registry,
            &state_store,
            &stats,
            session_started,
            theme,
            loop_state.as_ref().map(|s| s.view(false)),
        );
        send_or_flush(
            &out_tx,
            Outbound::View(view),
            &mut debug,
            &stats,
            run_rounds,
        )
        .await?;
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
/// - `Deferred`（后台子 agent 结果）**同样丢弃**，且断言它不会到这里：
///   后台任务随**回合**存活（runner 是回合块内的局部量），而本函数只在**命令**处理
///   期间被等待 —— 两者不重叠。若真到达，说明生命周期契约被破坏，本处丢弃**不静默**：
///   见下面的 `debug_assert`；
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
                Some(Boundary::Deferred { .. }) => {
                    // 不该到这里（后台任务只活在回合内，本函数只在命令期间等待）。
                    // **不静默**：debug 构建下直接暴露生命周期契约被破坏。
                    debug_assert!(
                        false,
                        "Deferred 在命令期间到达 = 后台任务的存活期跨过了回合边界"
                    );
                    continue;
                }
                Some(Boundary::Steer(_)) => continue,
                // 命令不是回合，没有轮边界可以消费压缩请求 ⇒ 丢弃。
                // 与 `Steer` 同理：命令结束后下一回合的起点会排空残留，
                // 故丢弃不改变任何已有语义（用户重敲一次即可）。
                Some(Boundary::Compact) => continue,
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

/// O1：换模型 / 换思考档位**前**的快照。
///
/// 承重的是「**前后两个快照都记**」—— 只记新的那份，就看不出「换过」。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelSnapshot {
    provider: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

/// 读当前的 provider / model / **生效**的思考档位，供「变没变」的比较。
///
/// 取 [`Config::effective_reasoning_effort`]（会话覆盖优先）而不是配置默认值：
/// `/effort` 改了档位也是一次变化，只记配置默认值会让它看不出来。
///
/// 这里存的是**原始档位**而非 wire 值：比较关心「用户想要的档位变了没有」，
/// 与该 provider / 模型代际实际发不发得出去无关（wire 值见 [`record_model_change`]）。
fn model_snapshot(config: &Config) -> ModelSnapshot {
    ModelSnapshot {
        provider: config.provider.clone(),
        model: Some(config.model.clone()),
        effort: config.effective_reasoning_effort(),
    }
}

/// O1：换模型 / 换思考档位时写一条 `model_changed` 事件。
///
/// **为什么需要它**：`run_start` 是**启动快照**（`main.rs` 的一次性写入），
/// **不反映运行期的 `/model` / `/login` 切换** ⇒ 光看 `run_start` 会误以为
/// 整个会话都跑在启动时那个 provider 上（真实会话里两者常常不同）。
///
/// **不重发 `run_start`**：那是 **run 级**事件（`run` 序号 + `begin_run`），
/// 重发会破坏 run 的划分与 `run_end` 的配对 ⇒ 另起一个事件类型。
///
/// 载荷取**前后两份快照** + **实际下发的 effort**（`None` = provider 或该模型代际
/// 不接受该字段 ⇒ 请求里不带）。
fn record_model_change(
    debug: &mut Option<DebugSink>,
    session_id: &str,
    before: &ModelSnapshot,
    config: &Config,
    cause: &str,
) {
    let Some(sink) = debug.as_mut() else {
        return;
    };
    let after = model_snapshot(config);
    // 没变就不记（避免 `/effort` 设成同值也刷一条）。
    if before == &after {
        return;
    }
    let mut fields = serde_json::Map::new();
    fields.insert("cause".into(), serde_json::json!(cause));
    fields.insert("from_provider".into(), serde_json::json!(before.provider));
    fields.insert("from_model".into(), serde_json::json!(before.model));
    fields.insert("to_provider".into(), serde_json::json!(after.provider));
    fields.insert("to_model".into(), serde_json::json!(after.model));
    // **实际下发的 effort**（门控 + 折叠之后；`None` = 请求里不带该字段）。
    //
    // 记 wire 值而不是原始配置值：GLM 的 `xhigh` 实际发 `max`，id 解析不出代际的模型则完全不发 ——
    // 原始值会让排障得出与真实请求相反的结论。
    //
    // 只有这一个字段：能力位（provider 级还是模型级、认不认该字段）已被它的
    // `null` / 非 `null` 完全表达，另记一个能力位会与 wire 值互相矛盾
    // （无法解析代际的模型上「provider 支持=true」+「effort=null」）。
    fields.insert(
        "reasoning_effort".into(),
        serde_json::json!(config.current_compat().effort_for(
            &config.model,
            config.effective_reasoning_effort().as_deref()
        )),
    );
    crate::debug_record(sink, session_id, "model_changed", None, None, fields);
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

/// 跑**一条回合链**（目前总是一回合：ADR-0041 撤回了完工复核）。
///
/// 两个调用点共用它：`Request::Prompt`（用户发言）、`Wake::BackgroundReturned` 与
/// `Wake::SubagentReturned`（后台结果自动起回合）。回合本身的推进/记账/写穿 meta 只有
/// 一份实现，否则两处必然漂移。
///
/// **wall-clock 预算在此算一次**：`deadline` 覆盖整条链，时长只来自
/// `config.max_turn_duration()`。
///
/// **`raw_user_text` 由调用方传入**：标题素材必须取**用户原文**（`/name` 展开前的
/// 那一行，C3）—— 展开后是 `<skill …>` 信封，不能当标题。harness 占位消息没有
/// 原文 ⇒ 传 `None`。
#[allow(clippy::too_many_arguments)]
async fn run_turn_chain(
    agent: &mut Agent,
    wiring: &mut Wiring,
    config: &Config,
    turns: &mut u32,
    stats: &mut TurnStats,
    thinking_visible: bool,
    run_rounds: &mut u32,
    out_tx: &mpsc::Sender<Outbound<CodingView>>,
    sink_rx: &mut mpsc::Receiver<Envelope>,
    boundary_rx: &mut mpsc::Receiver<Boundary>,
    subagent: Option<&crate::subagent::SubagentSetup>,
    debug: Option<&mut DebugSink>,
    debug_session_id: &str,
    worktree: &WorktreeHandle,
    message: Message,
    raw_user_text: Option<String>,
) -> Result<TurnOutcome, AppLoopError> {
    let request_started = Instant::now();
    let deadline = request_started + config.max_turn_duration();
    let subagent_totals = Arc::new(crate::subagent::UsageTotals::default());
    let mut ctx = TurnCtx {
        agent,
        wiring,
        config,
        turns,
        stats,
        thinking_visible,
        deadline,
        request_started,
        cumulative_rounds: 0,
        run_rounds,
        out_tx,
        sink_rx,
        boundary_rx,
        subagent,
        subagent_totals,
        debug,
        debug_session_id,
    };
    // 外层 `Err` = UI 已收摊（出站失败）：先收尾 debug 再退出；
    // 内层 `TurnOutcome` 的 `Err` 只是本回合模型/工具失败，app 继续。
    //
    // **返回值交给调用方**：追问判据需要 `StopReason`（跨回合推进设计 §3.5）。
    // 既有调用点忽略它，行为不变。
    match run_turn_once(&mut ctx, AgentInput::new(message)).await {
        Ok(outcome) => {
            // 用**引用**匹配：`outcome` 整个要交出去，不能在这里被消费。
            match &outcome {
                Ok(result) => {
                    ctx.stats.record_result(result);
                    // **执行口径**的时长累加：与上面的 usage 同一位置 ⇒
                    // 「token 统计」与「时长统计」覆盖同一批回合。
                    // 失败回合也计入（模型跑过了就是占用了墙钟）⇒ 见下。
                    ctx.stats.record_elapsed(ctx.request_started.elapsed());
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
                // **但时长要计入**：模型跑过了就是占用了墙钟 ⇒
                // 执行口径必须包含失败回合，否则「跑失败的时长」凭空消失。
                Err(e) => {
                    ctx.stats.record_elapsed(ctx.request_started.elapsed());
                    logging::log(&format!("turn {} failed: {e}", ctx.turns));
                }
            }
            Ok(outcome)
        }
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
            Err(e)
        }
    }
}

/// 后台结果唤醒的**复核判据**：本会话是否真的有已终态、尚未投出的结果。
///
/// 唤醒只说「**可能**有了」（清扫是进程级的，`notify` 也不携带会话信息）⇒ 起回合前
/// 必须复核。空 ⇒ 不起回合、不发文案（既省一次模型调用，也不制造噪音）。
fn should_start_background_turn(tasks: &BackgroundTasks, session: &str) -> bool {
    tasks.has_pending(session)
}

/// 定时循环起回合时的占位输入。
///
/// 它**不是用户说的话** ⇒ `Role::Harness`（同 [`background_placeholder`]）。
/// 正文用 `/loop` 给的提示词——**但前缀标出来源**，否则历史里看不出这一轮
/// 是被定时器叫醒的。
fn loop_placeholder(prompt: &str) -> Message {
    Message {
        role: Role::Harness,
        content: vec![ContentBlock::Text {
            text: format!("[harness] 定时检查：{prompt}"),
        }],
    }
}

/// 把剩余时长渲染成状态行的「还有多久」（`14 分后`）。
///
/// **给的是相对时长，不是钟点**：`Instant` 与系统钟点不可互推，而本地时区偏移
/// 在 `ys-tui-coding::clock` 里是 `pub(crate)`、本 crate 拿不到。给一个 UTC 钟点
/// 再冒充本地时间比不显示更糟 —— 用户会按错的时间预期下一轮。相对时长与机器
/// 时区无关且永远正确。
fn relative_label(d: std::time::Duration) -> String {
    // **向上取整**：截断会把「还有 0.9 秒」显示成「0 秒后」—— 一个不存在的时刻。
    // 对短间隔（10s 量级）这条差别直接决定状态行是否可信。
    let secs = d.as_secs().saturating_add(u64::from(d.subsec_millis() > 0));
    if secs < 60 {
        format!("{secs} 秒后")
    } else if secs < 3600 {
        format!("{} 分后", secs / 60)
    } else {
        format!("{} 时 {} 分后", secs / 3600, (secs % 3600) / 60)
    }
}

/// 后台结果自动起回合时的占位输入。
///
/// 它**不是用户说的话** ⇒ `Role::Harness`。`BasicLoop` 会照它 append 进会话
/// （与任何回合的输入同一条路径），TUI 据 `Role` 画出 System 行而非用户行。
///
/// 正文说清「这只是占位」：真正的内容是随后经 `Boundary::Deferred` 注入的那些结果，
/// 模型看到的是那几条，不是这一行。
fn background_placeholder() -> Message {
    Message {
        role: Role::Harness,
        content: vec![ContentBlock::Text {
            text: "[harness] 后台命令结果已送入上下文，请据此继续。".to_string(),
        }],
    }
}

/// 子 agent 结果自动起回合时的占位输入。
///
/// 与 [`background_placeholder`] 同一条理由：它**不是用户说的话** ⇒ `Role::Harness`。
/// 真正的内容是随后经 `Boundary::Deferred` 注入的那几条派发结果。
fn subagent_placeholder() -> Message {
    Message {
        role: Role::Harness,
        content: vec![ContentBlock::Text {
            text: "[harness] 子 agent 结果已送入上下文，请据此继续。".to_string(),
        }],
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
/// 但 `Steer` **不能一起丢** —— 它是用户输入正文，保留下来交给本回合（见排空处注释）。
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
    // 排空上一回合残留的边界消息 —— **只丢 `Abort`，保留 `Steer`**。
    //
    // `Abort` 必须丢：上一回合的取消**不得**取消本回合（它的标志位已永久置位，
    // 队列里那条已投递的消息是本回合唯一的补救点）。
    //
    // `Steer` **不能丢**：那是**用户输入的正文**。它会出现在这里只有一个原因 ——
    // 用户在上回合**最后一个轮边界之后**才敲进来，`BasicLoop` 没来得及取。
    // 丢掉 = 静默吞掉用户消息。保留下来泵进本回合的边界源，它会在本回合第一个
    // 轮边界被取走并 append 进会话 ⇒ 语义从「插话」自然退化为「下一回合的早到输入」，
    // 但**内容一个字不丢**。
    //
    // `Deferred`（后台子 agent 的结果）同理保留：那是**子 agent 已经跑完产出的内容**，
    // 丢掉等于白烧一次派发。
    //
    // `Compact`（手动压缩请求）同样保留：用户敲的 `/compact` 不能因为「恰好跨了
    // 回合边界」就静默失效 —— 它会在本回合第一个轮边界被取走并执行。
    let mut carry_over: Vec<Boundary> = Vec::new();
    while let Ok(b) = ctx.boundary_rx.try_recv() {
        if !matches!(b, Boundary::Abort) {
            carry_over.push(b);
        }
    }

    // 回合号从 1 起（`begin_turn` 把它带进 `Envelope.turn`）。
    *ctx.turns += 1;
    // 同一编号写进子 agent 的共享回合号：句柄 `sa{turn}.{n}` 的回合段取自它，
    // 跨回合不重复 ⇒ `subagent_send` 不会寻到上一个回合的同序号任务。
    if let Some(s) = ctx.subagent {
        s.set_turn(*ctx.turns);
    }
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
    ctx.agent
        .set_context_window(ctx.config.effective_context_window());
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
    // 本回合的后台清理句柄 —— **必须在借用 `wiring` 之前取出来**：
    // 回合内 `ctx.wiring.ports()` 是**可变借用**，持有期间碰不到 `ctx.wiring`。
    // 会话在回合内不变 ⇒ 会话 id 在回合开头取一次就够。
    let background = ctx
        .wiring
        .backgrounds()
        .map(|context| (context.tasks().clone(), context.session_id()));
    let outcome = {
        // 模型取**共享所有权**副本（异步派发的后台任务要 `'static`）；在 `ports()`
        // 的可变借用之前取，避免借用冲突。
        let model_arc = ctx.wiring.model_arc();
        let ports = ctx.wiring.ports();
        ports.events.begin_turn(*ctx.turns);
        // 本回合专属的边界源：`Abort` 的标志位随回合作废。
        let boundary = Arc::new(QueueBoundarySource::new());
        // 把上回合末尾没取走的 `Steer` 交给本回合（用户输入不能丢 —— 见函数头的排空契约）。
        for b in carry_over.drain(..) {
            boundary.push(b);
        }
        // 绑定本回合的**后台结果投递落点**（RAII：函数体内多处 `?` 提前返回，
        // 手动解绑必漏）。注册表是进程级的，而边界源每回合新建 ⇒ 落点只能逐回合绑。
        // 守卫在本块末尾 drop（早于随后的回合末事件排空）⇒ 排空期间不再有新投递。
        let _bg_binding = background
            .as_ref()
            .map(|(tasks, _)| tasks.bind_deferred(boundary.clone()));
        // 上一回合（或落点未绑的那一瞬）攺下的完成项：**一次取完**。
        //
        // 起一个回合已经付了模型调用，欠下的结果一次带完比「为剩下几条再起几轮」
        // 便宜；限流还会让「限流外的条目停在 `Pending`、而唤醒只在**新终态**时发」
        // 变成死角（再也没人唤醒它们）。上界由 `BgLimits::max_entries`（默认 8）与
        // 单条正文截断（`MAX_BYTES` = 50 KiB）给出，不需要额外的每回合限流。
        //
        // 取出的条目已被认领（`InFlight`）⇒ **每条都必须落状态**，否则它会卡到
        // 下一 tick 的修复才重投。渲染总是给出正文（输出读不出时是「不可读」形态），
        // 故这里不存「空正文」分支：空正文会让模型以为「命令没有输出」。
        if let Some((tasks, session)) = background.as_ref() {
            for candidate in tasks.take_completions(session) {
                boundary.push(Boundary::Deferred {
                    handle: candidate.handle.label(),
                    message: render_completion(&candidate),
                });
                tasks.finish_delivery(candidate.handle, true);
            }
        }
        // **上一回合遗留的子 agent 结果：一次性补投**。
        //
        // 子 agent 是分离任务（`tokio::spawn`），回合结束后仍会继续跑（实测转录
        // 在回合结束后 2~6 分钟才落盘）⇒ 完成时本回合的边界源已销毁，结果留表。
        // 不在这里补投，模型就永远等不到它 —— 那正是「派发白烧一次、模型反复说
        // 还在跑」的故障（会话 `1792840206_799154000`）。
        //
        // 与后台命令的补投同形，但**不限流**：子 agent 每个都是一次真模型调用，
        // 数量由 `DEFAULT_MAX_DISPATCHES_PER_TURN` 天然封顶（≤16），且每个结果都在
        // 等一个明确答复 —— 限流只会让模型继续误以为「还没回来」。
        if let Some(s) = ctx.subagent {
            for (handle, message) in s.take_pending_results() {
                boundary.push(Boundary::Deferred { handle, message });
            }
        }
        // subagent：每回合造一份 runner（派发计数器天然 per-turn；每次派发建新子 Agent）。
        // 父边界源与 `AgentPorts` 共用**同一份** `Arc` ⇒ 子 agent 的 `ChildBoundary`
        // 透传得到父的中止状态。
        //
        // 后台结果的落点**每回合绑定**：投递表是进程级的（子 agent 可能在本回合
        // 结束后才完成，`tokio::spawn` 是分离任务），而边界源每回合新建 ⇒ 不绑就会
        // 把结果推进一个无人读的队列（看起来成功，实则永久沉底）。
        //
        // **守卫必须活到块末**（绑在 `_subagent_binding` 而非 `_`）：写 `_` 会立即
        // drop ⇒ 解绑 ⇒ 本回合后半程的完成项又只能留表、白等一个回合。
        let _subagent_binding = ctx.subagent.map(|s| s.bind_results(boundary.clone()));
        let runner = ctx.subagent.map(|s| {
            s.make_runner(
                Some(ctx.config),
                model_arc.clone(),
                ctx.subagent_totals.clone(),
                Some(boundary.clone()),
            )
        });
        let mut agent_ports = AgentPorts::new(
            ports.model,
            ports.session,
            ports.events,
            Some(boundary.clone()),
        );
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
                    if matches!(&b, Boundary::Abort) {
                        // **清理点（abort）**：本会话的后台命令进程组一律杀掉
                        // （按会话过滤 ⇒ 别的会话不受影响）。
                        if let Some((tasks, session)) = background.as_ref() {
                            tasks.kill_all(session);
                        }
                        if let Some(sink) = ctx.debug.as_deref_mut() {
                            crate::record_boundary(
                                sink,
                                ctx.debug_session_id,
                                Some(*ctx.turns),
                                "enqueued",
                                "abort",
                                Some("ui"),
                            );
                        }
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
                    // **清理点（回合预算到点）**：语义与 abort 一致 ——
                    // 本会话的后台命令进程组一律杀掉（按会话过滤）。
                    if let Some((tasks, session)) = background.as_ref() {
                        tasks.kill_all(session);
                    }
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
            let (subagent, subagent_cache) = ctx.subagent_totals.drain();
            // 缓存读数也要上卷。`subagent_cache` 是累加器**逐次**累加出来的
            // `(hit, miss)`，不能从 `subagent`（聚合 `Usage`）派生 ——
            // 那里的 `input` 对没上报的子调用也累加过，减法会把它们算成未命中。
            if let Some((hit, miss)) = subagent_cache {
                r.cache_reading = Some(match r.cache_reading {
                    Some((h, m)) => (h.saturating_add(hit), m.saturating_add(miss)),
                    None => (hit, miss),
                });
            }
            r.usage = r.usage + subagent;
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
    // 回合级读数：**只进 debug / 日志**（供 bench 与事后分析）；
    // 不占 transcript —— TUI 用户要的是对话，读数在状态栏与 `/status` 里。
    let reading = clock.summary_line(*ctx.turns);

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
    // 回合末读数行**不经 `Outbound::Output`**：transcript 只装对话与命令输出。

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
        // 回合级读数行只落 debug（不进 transcript / stderr）。
        crate::record_run_reading(sink, ctx.debug_session_id, *ctx.turns, &reading);
        // 回合末的后台命令计数 —— **只进 debug 日志**：不进模型上下文、
        // 不进 transcript（绝不 `Outbound::Output`）。N = 本会话**仍在跑**的
        // 后台命令数。
        let running = background
            .as_ref()
            .map(|(tasks, session)| tasks.running_handles(session).len())
            .unwrap_or(0);
        let mut bg_fields = serde_json::Map::new();
        bg_fields.insert("running".into(), serde_json::json!(running));
        crate::debug_record(
            sink,
            ctx.debug_session_id,
            "background_tasks",
            Some(*ctx.turns),
            None,
            bg_fields,
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
/// - 首块**不是 `Text`**（例如首块即图片）⇒ **整条不展开**（只认纯文本首块的 `/` 命名空间）。
///   这是刻意保留的既有语义：`/name` 只在「用户敲的第一段是命令」时成立；放宽到
///   任意位置会把正文中途出现的 `/foo`、以及 `[Image, Text("/foo")]` 这类组合
///   也当命令解析。首块非 `Text` 时消息**原样返回**，块一个不丢。
/// - 未知命令 / `user-invocable: false` / 正文读不出来 ⇒ `expand_invocation` 返回
///   `None` ⇒ 原样返回（宁可发一条普通消息，也不塞半截指令）。
/// - **不缓存**正文：skill 目录可写（agent 会往里写文件），缓存会给出陈旧指令。
///
/// # 只替换首个 `Text` 块，其余块原位保留
///
/// 展开结果**只占首个 `Text` 块的位置**；`Image` 等其余块原封不动、且**下标不变**
/// （`[Text(…), Image, Text(尾巴)]` ⇒ `[Text(信封), Image, Text(尾巴)]`）。
/// 绝不可把消息重建成「只有展开结果一个 `Text` 块」—— 那会**静默丢图**：用户发
/// 「`/skill` + 一张截图」时图片在无编译错误、无运行时告警的情况下从上下文里消失。
fn expand_skill_invocation(message: Message, skills: &[crate::skills::SkillEntry]) -> Message {
    // skill 调用是**用户输入**的概念：只有 `Role::User` 才展开。harness 注入的输入
    // （后台命令结果等）与用户原文可以长得一样，但它不是用户在敲命令 —— 展开路径
    // 写的是 `Role::User` 的用户发言语义。
    if message.role != Role::User {
        return message;
    }
    let Some(text) = first_text(&message) else {
        return message;
    };
    match crate::skills::expand_invocation(&text, skills) {
        Some(expanded) => {
            logging::log(&format!(
                "skill invocation expanded ({} bytes) for this turn",
                expanded.len()
            ));
            let mut message = message;
            // 首个 `Text` 块换成展开结果，其余块（图片等）**保持原位**。
            // `first_text` 已确认首块是 `Text`，所以这个循环必然在第一轮命中并 `break`；
            // 写成循环而非 `message.content[0] = …` 是为了不把「首块即 Text」这个前提
            // 变成隐式下标依赖。
            for block in &mut message.content {
                if let ContentBlock::Text { text } = block {
                    *text = expanded;
                    break;
                }
            }
            message
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
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
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
        /// 后台命令上下文（`None` = 本轮没挂后台能力）——
        /// 注册表与**会话槽**（槽与接线器共享同一份 ⇒ 清理点与工具看到的
        /// 会话 id 必然一致）。
        backgrounds: Option<(Arc<BackgroundTasks>, ys_tools_basic::SessionSlot)>,
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
            false,
        )
        .await
    }

    /// 同 [`spawn_app_loop_debug`]，但允许在起跑前改写 [`Config`]（第二个参数是
    /// 临时目录）—— 供「启动态就已登录某个 provider / 某个模型代际」的用例把
    /// 现场摆好，而不是绕道 `Request::Login`（那会触发真实网络与目录拉取）。
    async fn spawn_app_loop_debug_with(
        model: Box<dyn Model>,
        persistent: bool,
        tweak: impl FnOnce(&mut Config, &PathBuf),
    ) -> Harness {
        spawn_app_loop_full(
            model,
            persistent,
            Vec::new(),
            tweak,
            |b| b,
            |_| None,
            None,
            true,
            true,
            false,
        )
        .await
    }

    /// 起 app 循环并挂上**后台命令上下文**（注册表 + 会话槽）。
    ///
    /// 注册表落在 harness 的临时目录里 ⇒ 用例结束随临时目录一起清掉。
    /// 会话槽与接线器共享**同一份** ⇒ 工具、清理点、测试三方看到的会话 id
    /// 必然一致。
    ///
    /// **调用方必须已持有 `env_lock`**（见 [`BgEnv`]）⇒ harness 的 `env_prelocked`
    /// 传 `true`（否则它会自己去抢同一把锁 ⇒ 同线程二次加锁死锁）。
    async fn spawn_app_loop_backgrounds(model: Box<dyn Model>, persistent: bool) -> Harness {
        spawn_app_loop_backgrounds_with(model, persistent, |_, _| {}).await
    }

    /// 同上，但允许改写 `Config`（第二个参数是临时目录）。
    async fn spawn_app_loop_backgrounds_with(
        model: Box<dyn Model>,
        persistent: bool,
        tweak: impl FnOnce(&mut Config, &PathBuf),
    ) -> Harness {
        spawn_app_loop_full(
            model,
            persistent,
            Vec::new(),
            tweak,
            |b| b,
            |_| None,
            None,
            false,
            true,
            true,
        )
        .await
    }

    /// 同上，但**打开 debug sink**（调用方须整段持有 `env_lock`）。
    async fn spawn_app_loop_debug_backgrounds(model: Box<dyn Model>, persistent: bool) -> Harness {
        spawn_app_loop_full(
            model,
            persistent,
            Vec::new(),
            |_, _| {},
            |b| b,
            |_| None,
            None,
            true,
            true,
            true,
        )
        .await
    }

    /// 最底层起法：除 `Config` 外还允许改写 [`ys_runtime::AgentBuilder>]
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
        // 挂上后台命令上下文（注册表 + 会话槽）。
        with_backgrounds: bool,
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
        // `Config::from_env` 会 `getenv` 凭证 key，而 `config.rs` 的用例正在
        // `env_lock` 下 `set_var` / `remove_var` —— 只读也须串行。
        //
        // `env_prelocked = true` 表示**调用方已持有 `env_lock`**（该锁是不可重入的
        // `std::sync::Mutex`，此处再锁会**同线程死锁**）⇒ 直接取用。
        // 锁不跨下方任何 `.await`：`MutexGuard` 非 `Send`，且无故长持全局锁。
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

        let mut wiring = if persistent {
            Wiring::persistent_new(Some(model), Box::new(sink), tmp.join("sessions"))
                .await
                .unwrap()
        } else {
            Wiring::ephemeral(Some(model), Box::new(sink))
        };

        // 后台命令上下文（可选）：注册表落在 harness 的临时目录里 ⇒ 用例结束
        // 随临时目录一起清掉。`attach_backgrounds` 会把会话槽同步到**当前**会话
        // ⇒ 工具、清理点、测试三方看到的会话 id 必然一致。
        let backgrounds = if with_backgrounds {
            let tasks = Arc::new(BackgroundTasks::new(
                tmp.join("bg-root"),
                ys_tools_basic::BgLimits::default(),
            ));
            // 完成正文的渲染回调：生产侧由 `main` 装载，未装载则清扫不投递。
            tasks.set_completion_renderer(ys_tools_basic::render_completion);
            let slot = ys_tools_basic::SessionSlot::new("pending");
            wiring.attach_backgrounds(crate::wiring::BackgroundContext::new(
                Arc::clone(&tasks),
                slot.clone(),
            ));
            Some((tasks, slot))
        } else {
            None
        };

        // 与生产侧同规：debug sink 在 `Wiring` 建好、会话路径已知之后打开，
        // 并由「启动期一方」写第一条 `run_start`。关闭时 `open_enabled` 早退。
        // 两条路径都必须持锁：`resolve_log_dir` 与 `debug_enabled` 都读环境变量。
        // `env_prelocked` 的调用方**已**持锁（同一把），这里直接取用。
        let mut debug = if env_prelocked {
            let log_dir = crate::logging::resolve_log_dir();
            crate::debug_log::open_enabled(debug_flag, wiring.session_path(), log_dir.as_ref())
        } else {
            debug_assert!(!debug_flag, "debug-on 用例必须自行隔离 YUSHAN_LOG_DIR");
            let _guard = crate::test_env::env_lock();
            let _env = crate::test_env::EnvRestore::capture(&[crate::debug_log::DEBUG_ENV]);
            // SAFETY: 持有 env_lock；默认关闭 debug 的 harness 不继承开发机环境。
            unsafe { std::env::set_var(crate::debug_log::DEBUG_ENV, "0") };
            let log_dir = crate::logging::resolve_log_dir();
            crate::debug_log::open_enabled(false, wiring.session_path(), log_dir.as_ref())
        };
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
            ys_tools_todo::TodoStore::open(None),
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
            backgrounds,
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
            cache_reading: None,
            compaction_usage: ys_core::Usage::default(),
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
        request.messages.iter().any(|m| has_text_in(m, needle))
    }

    /// 单条消息里是否有包含 `needle` 的文本块。
    fn has_text_in(message: &Message, needle: &str) -> bool {
        message
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text.contains(needle)))
    }

    /// 记录每次模型调用的请求 —— 供「后台结果是否自动起了回合」取证。
    #[derive(Default)]
    struct CallLog {
        calls: AtomicU32,
        requests: Mutex<Vec<ModelRequest>>,
    }

    impl CallLog {
        fn count(&self) -> u32 {
            self.calls.load(Ordering::Relaxed)
        }

        fn requests(&self) -> Vec<ModelRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    /// 每次调用都立刻收工的模型：回合数=1，便于精确断言「是否多起了回合」。
    struct CountingModel(Arc<CallLog>);

    #[async_trait::async_trait]
    impl Model for CountingModel {
        fn model_id(&self) -> &str {
            "counting"
        }

        async fn complete(
            &self,
            request: ModelRequest,
            _sink: &mut dyn ModelEventSink,
        ) -> Result<ModelResponse, ModelError> {
            self.0.calls.fetch_add(1, Ordering::Relaxed);
            self.0.requests.lock().unwrap().push(request);
            Ok(text_response("ok"))
        }
    }

    /// 把本回合收到的 `DeferredResult` 按回合号归组。
    fn deferred_by_turn(
        msgs: &[Outbound<CodingView>],
        into: &mut std::collections::BTreeMap<u32, String>,
    ) {
        for m in msgs {
            let Outbound::Event(env) = m else { continue };
            let AgentEvent::DeferredResult { message, .. } = &env.event else {
                continue;
            };
            let text: String = message
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            into.entry(env.turn).or_default().push_str(&text);
        }
    }

    /// 等「**某一个回合**里集齐了 `needles` 全部结果」，返回该回合号与该回合的正文。
    ///
    /// 后台结果只能经 `Boundary::Deferred` 进上下文（同一条路径 append + 进本轮
    /// `ModelRequest`）⇒ 按回合归组就能判定「一次取完」，无需给模型塞探针。
    async fn wait_for_one_turn_with(
        h: &mut Harness,
        already: Vec<Outbound<CodingView>>,
        needles: &[&str],
        limit: Duration,
    ) -> (u32, String) {
        let mut by_turn = std::collections::BTreeMap::new();
        deferred_by_turn(&already, &mut by_turn);
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            if let Some((&turn, text)) = by_turn
                .iter()
                .find(|(_, text)| needles.iter().all(|n| text.contains(n)))
            {
                return (turn, text.clone());
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !left.is_zero(),
                "后台结果没有在**同一个回合**里集齐（每回合限流未拆除？已有：{by_turn:?}）"
            );
            let m = tokio::time::timeout(left, h.outbound.recv())
                .await
                .expect("等待 Outbound 超时")
                .expect("app 循环不应提前收摊");
            deferred_by_turn(&[m], &mut by_turn);
        }
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

        // 回合 1：事件（种类顺序固定）→ View。读数**不经出站**（只落 debug）。
        h.requests.send(prompt("one")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        assert_eq!(view.turn_count, 1);
        assert!(
            !msgs.iter().any(|m| matches!(m, Outbound::Output(_))),
            "回合结束时不得再有读数 `Output`（读数只落 debug）：{msgs:?}"
        );
        assert!(
            msgs.iter().all(|m| matches!(m, Outbound::Event(_))),
            "回合期间只应有事件与末尾 View：{msgs:?}"
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

    /// O1：换模型后 debug 日志里必须有一条 `model_changed` 事件。
    ///
    /// 承重的是「**运行期换模型这件事本身要可观测**」——
    /// `run_start` 只是**启动快照**，不反映 `/model` / `/login` 的切换。
    ///
    /// ⚠️ **必须把 `YUSHAN_LOG_DIR` 指到临时目录**：默认档是 `~/.yushan/logs`
    /// ⇒ 同进程内并行测试的 debug 文件名都是 `debug-<pid>-<ts>.jsonl` ⇒ **撞名互相覆盖**
    /// ⇒ 用例变成 flaky（实测 2/3 次失败）。
    ///
    /// 变异：删掉 `record_model_change(...)` 调用 ⇒ 变红。
    #[tokio::test]
    async fn model_switch_is_observable_in_debug_log() {
        let logs = TempDir::new("model_changed_logs");
        let _env = DebugEnv::set(&logs.0);

        let model = MockModel::new("mock");
        model.push_text("a");
        let mut h = spawn_app_loop_debug(Box::new(model), false, Vec::new()).await;

        h.requests
            .send(Request::SetModel {
                model: "another-model".into(),
                provider: Some("deepseek".into()),
            })
            .await
            .unwrap();
        // 换模型后会刷一次 view；收它以确保处理完。
        let _ = collect_until_view(&mut h).await;
        shutdown(h).await;

        // debug writer 是异步线程 ⇒ 轮询等文件出现。
        let mut found = false;
        for _ in 0..100 {
            for (_path, records) in debug_files(&logs.0) {
                if let Some(record) = records
                    .iter()
                    .find(|r| r.get("ev").and_then(|e| e.as_str()) == Some("model_changed"))
                {
                    found = true;
                    // 载荷必须带**前后**两份快照 + 实际下发的 effort。
                    let raw = serde_json::to_string(&records).unwrap();
                    assert!(raw.contains("from_provider"), "缺 from_provider");
                    assert!(raw.contains("to_model"), "缺 to_model");
                    assert!(
                        raw.contains("reasoning_effort"),
                        "缺 reasoning_effort（实际下发值）"
                    );
                    // 能力位已由 `reasoning_effort` 的 `null` / 非 `null` 表达；
                    // 另记一个 provider 级能力位会与 wire 值互相矛盾。
                    assert!(
                        record.get("supports_reasoning_effort").is_none(),
                        "不应再记录 provider 级能力位：{record}"
                    );
                    break;
                }
            }
            if found {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(found, "换模型后应有 model_changed 事件；目录：{:?}", logs.0);
    }

    /// 跑一次「换到 `provider` / `model`」，返回该次 `model_changed` 里的
    /// `reasoning_effort` 字段（`null` = 请求里不带该字段）。
    ///
    /// 启动态由 `tweak` 把 **provider + 档位**摆好、模型名留成占位值，再靠
    /// `SetModel` 只改模型 ⇒ 恰好触发一条 `model_changed`（`/effort` 与 `/model`
    /// 走同一 `record_model_change`），且不依赖开发机的 `YUSHAN_REASONING_EFFORT`。
    ///
    /// 每个 case 用**独立日志目录**：无会话文件时 debug 名是
    /// `debug-<pid>-<ms>.jsonl`，同进程内多 case 共目录会互相串。
    async fn model_changed_wire_effort(
        tag: &str,
        provider: &str,
        model: &str,
        effort: ys_core::EffortLevel,
    ) -> serde_json::Value {
        let logs = TempDir::new(tag);
        // `DebugEnv` 整段持有 `env_lock`：必须在本次调用内 drop，不能跨 case 持有。
        let _env = DebugEnv::set(&logs.0);

        let mock = MockModel::new("mock");
        let mut h = spawn_app_loop_debug_with(Box::new(mock), false, |config, _tmp| {
            config.provider = Some(provider.to_string());
            config.effort = Some(effort);
            config.model = "before-switch".to_string();
        })
        .await;

        // `provider` 与启动态相同 ⇒ `set_model` 不走换 provider 的凭证分支，
        // 不触发任何网络；只有模型名变化。
        h.requests
            .send(Request::SetModel {
                model: model.to_string(),
                provider: Some(provider.to_string()),
            })
            .await
            .unwrap();
        let _ = collect_until_view(&mut h).await;
        shutdown(h).await;

        for _ in 0..100 {
            for (_path, records) in debug_files(&logs.0) {
                if let Some(record) = records
                    .iter()
                    .find(|r| r.get("ev").and_then(|e| e.as_str()) == Some("model_changed"))
                {
                    return record["reasoning_effort"].clone();
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("换模型后应有 model_changed 事件；目录：{:?}", logs.0);
    }

    /// `model_changed.reasoning_effort` 记的是 **wire 值**（`None` = 请求里不带该字段）。
    ///
    /// 承重的是「日志与真实请求一致」：记原始配置值会让排障得出相反结论 ——
    /// GLM 的 `xhigh` 在 5.3 上实际发 `max`，在 5.2 上同样是 `max`，更早代际原样发 `xhigh`，
    /// 而 id 解析不出代际的模型则完全不发。
    ///
    /// 变异：把 `effort_for(&config.model, ...)` 换回
    /// `config.effective_reasoning_effort()` ⇒ 第一条断言拿到 `"xhigh"` 而非
    /// `"max"`、第三条拿到 `"xhigh"` 而非 `null` ⇒ 变红。
    #[tokio::test]
    async fn model_changed_records_wire_effort() {
        // GLM 5.3（≥ 5.3 的代际）把 `xhigh` 折叠成 `max`。
        let glm53 = model_changed_wire_effort(
            "wire_effort_glm53",
            "glm",
            "glm-5.3",
            ys_core::EffortLevel::XHigh,
        )
        .await;
        assert_eq!(
            glm53,
            serde_json::json!("max"),
            "glm-5.3 的 xhigh 在 wire 上是 max"
        );

        // 5.3 之前的代际（这里用 `glm-5`）原样透传 `xhigh`。
        let glm_old = model_changed_wire_effort(
            "wire_effort_glm_old",
            "glm",
            "glm-5",
            ys_core::EffortLevel::XHigh,
        )
        .await;
        assert_eq!(
            glm_old,
            serde_json::json!("xhigh"),
            "5.2 之前的代际原样透传 xhigh，得到 {glm_old}"
        );

        // id 解析不出 GLM 代际（provider 仍为 `glm`）⇒ 不发该字段 ⇒ 日志必须是 `null`。
        let unknown = model_changed_wire_effort(
            "wire_effort_glm_unknown",
            "glm",
            "some-unknown-id",
            ys_core::EffortLevel::XHigh,
        )
        .await;
        assert!(
            unknown.is_null(),
            "模型级门控 fail-closed 时不发该字段，日志应为 null，得到 {unknown}"
        );

        // 逐字方言不折叠：GLM 的折叠规则不得外溢到其它 provider。
        let opencode = model_changed_wire_effort(
            "wire_effort_opencode",
            "opencode-go",
            "deepseek-v4-pro",
            ys_core::EffortLevel::XHigh,
        )
        .await;
        assert_eq!(
            opencode,
            serde_json::json!("xhigh"),
            "opencode-go 原样透传 xhigh"
        );
    }

    /// A1 的端到端不变量：跑完一个回合后，**执行时长**必须大于零。
    ///
    /// 承重的是「累加点在回合末」—— 若漏掉（或只记成功不记失败），
    /// 执行口径会**小于**真实占用 ⇒ `/status` 的两个口径失去意义。
    ///
    /// 变异：删掉 `ctx.stats.record_elapsed(...)` ⇒ 变红。
    #[tokio::test]
    async fn executed_duration_grows_after_a_turn() {
        let model = MockModel::new("mock");
        model.push_text("a");
        let mut h = spawn_with_mock(model).await;

        h.requests.send(prompt("hi")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);

        assert!(
            view.executed > std::time::Duration::ZERO,
            "跑完一个回合后执行时长必须大于零（实际 {:?}）",
            view.executed
        );
    }

    /// **上回合末尾没取走的 `Steer` 不得被丢弃** —— 它是用户输入正文。
    ///
    /// 场景：用户在上回合**最后一个轮边界之后**才敲进来 ⇒ `BasicLoop` 没来得及取 ⇒
    /// 它躺在 `boundary_rx` 里。下一个 `Request::Prompt` 到达时，`run_turn_once` 的
    /// 排空循环若「见消息就丢」，用户那句话就**静默消失**了。
    ///
    /// 修法：排空**只丢 `Abort`**，`Steer` 带进本回合的边界源 ⇒ 在**第一个轮边界**
    /// （模型调用之前）被取走并 append ⇒ 语义自然从「插话」退化为「下一回合的早到输入」，
    /// 内容一个字不丢。
    ///
    /// 变异：把排空改回 `while try_recv().is_ok() {}`（全丢）⇒ 本用例变红。
    #[tokio::test]
    async fn leftover_steer_is_carried_into_the_next_turn() {
        const MARKER: &str = "CARRIED-OVER-STEER";
        let (model, mut probe) = probe_model(ProbePlan::CompleteAtCalls(&[1, 2]));
        let mut h = spawn_app_loop(Box::new(model), false, Vec::new()).await;

        // 回合 1：正常跑完。**第 1 次 `complete` 恒阻塞在 gate 上**（probe 的约定）⇒
        // 必须显式放行，否则整个用例卡死在第 1 轮。
        h.requests.send(prompt("first")).await.unwrap();
        probe.wait_entered(1).await;
        probe.release();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);

        // 回合 1 已结束 ⇒ 现在投的 Steer 没有任何轮边界去取它。
        h.boundaries
            .send(Boundary::Steer(user_message(MARKER)))
            .await
            .unwrap();
        // 给 app 循环一个 poll 周期把它收进边界队列（它此刻不在回合内，只入队）。
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // 回合 2：排空时**必须**把上面那条 Steer 带进来。
        h.requests.send(prompt("second")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);

        let requests = probe.requests();
        assert!(
            requests.len() >= 2,
            "应至少跑两轮（两个回合）：{}",
            requests.len()
        );
        assert!(
            has_text(&requests[1], MARKER),
            "回合 2 的第 1 轮请求里必须已经带上被携带过来的 Steer（不能丢用户输入）"
        );
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

    /// `/clear`：清屏 + 清上下文 + **旧会话文件一字不动**。
    ///
    /// 两条硬契约：
    /// 1. `SessionCleared` **先于** `Output` —— 反了的话 UI 的清屏会把提示行一起吃掉；
    /// 2. 换新文件而不是原地清空 —— `JsonlSession::append` 全量重写目标文件，
    ///    原地清空后再发消息就会把旧内容永久覆盖。
    ///
    /// 变异：把 `session_event = Some(Outbound::SessionCleared)` 挪到 `Output` 之后
    /// （或改成不设）⇒ 顺序断言变红；把「换新文件」改成原地 `session.clear()` ⇒
    /// `old_path` 仍带 2 条消息那条断言变红（路径不变且被重写）。
    #[tokio::test]
    async fn clear_session_keeps_old_file_and_blanks_screen() {
        let model = MockModel::new("mock");
        model.push_text("hello");
        let mut h = spawn_app_loop(Box::new(model), true, Vec::new()).await;

        h.requests.send(prompt("hi")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let before = take_view(&mut msgs);
        assert_eq!(before.message_count, 2, "user + assistant");
        let old_path = before.session_path.clone().expect("持久会话应有文件");
        assert!(old_path.exists(), "回合结束后会话文件应落盘");

        h.requests.send(Request::ClearSession).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let after = take_view(&mut msgs);

        // ① 出站顺序：`SessionCleared` 在前，`Output` 在后（I2）。
        match msgs.as_slice() {
            [Outbound::SessionCleared, Outbound::Output(_)] => {}
            other => panic!("出站序列应为 [SessionCleared, Output]：{other:?}"),
        }
        // ② 屏空白。
        assert_eq!(after.message_count, 0, "/clear 后上下文归零");
        // ③ 旧文件仍在，且内容一字未丢。
        assert!(old_path.exists(), "/clear 必须保留旧会话文件");
        let reopened = ys_session::JsonlSession::open(&old_path).await.unwrap();
        use ys_session::Session as _;
        assert_eq!(
            reopened.messages().len(),
            2,
            "旧文件不得被重写：{old_path:?}"
        );
        // ④ 换到了新文件。
        let new_path = after.session_path.clone().expect("持久会话应有文件");
        assert_ne!(old_path, new_path, "/clear 应换新会话文件");
        assert_eq!(
            h.session_path.get().as_ref(),
            Some(&new_path),
            "/clear 后退出提示快照应指向新会话文件"
        );
    }

    /// `/clear` 失败：出站序列只有 `Output`（一行错误），**不发** `SessionCleared`。
    ///
    /// 理由：`SessionCleared` 会让 UI 清屏；换会话失败时历史仍在 ⇒ 清了屏就是
    /// 「屏空了、历史还在」的错位。
    ///
    /// 失败构造：回合跑完后把 harness 的 `sessions/` 目录换成一个**普通文件** ⇒
    /// `Wiring::new_session` 的建目录/开新文件必然失败（无需 chmod，跨平台一致）。
    ///
    /// 变异：把 `if ok` 去掉、改成无条件 `session_event = Some(Outbound::SessionCleared)`
    /// ⇒ 首条断言（出站序列）变红；把 `capabilities::clear_session` 的 `Err` 臂
    /// 改成 `(lines, true)` ⇒ 同上。
    #[tokio::test]
    async fn clear_session_failure_sends_no_session_cleared() {
        let model = MockModel::new("mock");
        model.push_text("hello");
        let mut h = spawn_app_loop(Box::new(model), true, Vec::new()).await;

        h.requests.send(prompt("hi")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let before = take_view(&mut msgs);
        assert_eq!(before.message_count, 2, "user + assistant");
        let old_path = before.session_path.clone().expect("持久会话应有文件");
        assert!(old_path.exists(), "回合结束后会话文件应落盘");

        // 让下一次换会话必然失败：把落盘目录换成一个普通文件。
        let dir = h.tmp.join("sessions");
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::write(&dir, b"not a dir").unwrap();

        h.requests.send(Request::ClearSession).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let after = take_view(&mut msgs);

        match msgs.as_slice() {
            [Outbound::Output(line)] => assert!(
                line.contains("Failed to clear context"),
                "失败应报一行错误：{line}"
            ),
            other => panic!("失败时出站序列应恰为 [Output]，不得发 SessionCleared：{other:?}"),
        }
        assert_eq!(after.message_count, 2, "失败后历史必须原样留着");
        assert_eq!(after.session_path, Some(old_path), "失败时不得换会话文件");
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

    /// **C8（写方向）**：`Request::SetTheme` 真的落盘、真的更新进程级镜像。
    ///
    /// 这条走**生产路径**（app 循环的 `SetTheme` 臂），不像 view 侧那条由测试自己
    /// 写 state.json。缺了它，把整个臂删成空操作、或只落盘不更新镜像，都无人发现。
    ///
    /// 变异：把该臂改成 `let _ = (name, kind);` ⇒ 本用例红。
    #[tokio::test]
    async fn theme_request_persists_and_updates_mirror() {
        let model = MockModel::new("mock");
        let mut h = spawn_app_loop(Box::new(model), false, Vec::new()).await;

        // ① 合法名：落盘 + 镜像更新（镜像经下一份 View 快照可见）。
        h.requests
            .send(Request::SetTheme {
                theme: "nord".into(),
            })
            .await
            .unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        assert_eq!(view.theme, ThemeKind::Nord, "镜像应更新，并经快照传出");

        let state_path = h.temp_path("state.json");
        let raw = std::fs::read_to_string(&state_path).expect("应已写出 state.json");
        assert!(raw.contains("\"theme\""), "state.json 应含 theme 键：{raw}");
        let saved: crate::state::AppState = serde_json::from_str(&raw).unwrap();
        assert_eq!(saved.theme.as_deref(), Some("nord"));

        // ② 大小写 / 下划线归一化：落盘的是规范名。
        h.requests
            .send(Request::SetTheme {
                theme: "GRUVBOX_DARK".into(),
            })
            .await
            .unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        assert_eq!(view.theme, ThemeKind::GruvboxDark);
        let raw = std::fs::read_to_string(&state_path).unwrap();
        assert!(raw.contains("\"gruvbox-dark\""), "落盘应为规范名：{raw}");

        // ③ 非法名：只警告、**不落盘**（文件保持上一次的合法值）。
        h.requests
            .send(Request::SetTheme {
                theme: "no-such-theme".into(),
            })
            .await
            .unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        assert_eq!(view.theme, ThemeKind::GruvboxDark, "非法名不得改镜像");
        let raw = std::fs::read_to_string(&state_path).unwrap();
        assert!(
            raw.contains("\"gruvbox-dark\""),
            "非法名不得覆写 state.json：{raw}"
        );

        shutdown(h).await;
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

    /// 6. **`Request` 变体覆盖**：13 个变体中本用例发 8 个（`Prompt` + 6 个能力 +
    ///    `SetThinking`；其余五个各另有用例：`ListSessions` / `ResumeSession` 需额外前置
    ///    条件，`Compact` 产出两条输出，`ClearSession` 额外发一条 `SessionCleared`，
    ///    `SetTheme` 只在主题名非法时警告一条），
    ///    app 循环都必须处理并给出出站消息、不得 panic（`Prompt` → 事件；6 个能力 →
    ///    `Output` + `View`；`SetThinking` → 仅 `View`）。
    #[tokio::test]
    async fn every_request_variant_is_handled() {
        let model = MockModel::new("mock");
        model.push_text("hi");
        let mut h = spawn_app_loop(Box::new(model), true, Vec::new()).await;
        let export_path = h.temp_path("exported.jsonl");

        // 除 `Prompt` 外、会产出 `Output` 的变体共 11 个：下表 6 个恰好一条；
        // `Compact` 两条（开始行 + 收场行）；`ClearSession` 先 `SessionCleared` 再 `Output`；
        // `ListSessions` / `ResumeSession` 需额外前置条件；`SetTheme` 只在主题名非法时
        // 警告一条 —— 后五者各自另有用例。
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
            // ⚠️ `Compact` **不在**这个表里：它是唯一会产出**两条** `Output` 的变体
            // （开始行在压缩那次 `await` 之前落地，见它的专门用例）。
            // ⚠️ `ClearSession` 也**不在**：它成功时先发一条 `SessionCleared`，
            // 出站序列是 `[SessionCleared, Output]` 而不是 `[Output]`；顺序是硬契约
            // （反了清屏会吃掉提示行），由 `clear_session_keeps_old_file_and_blanks_screen`
            // 专门钉住。
        ];
        assert_eq!(cases.len(), 6, "6 个恰好输出一条 Output 的能力变体");

        // `Prompt` 先跑：会话有内容，`/export` 才有东西可导。
        h.requests.send(prompt("hello")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);
        // 回合末不再有读数 `Output`；其余仍只有事件。
        assert!(
            !msgs.iter().any(|m| matches!(m, Outbound::Output(_))),
            "回合结束时不得再有读数 `Output`：{msgs:?}"
        );
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

    /// **空闲 `/compact` 产出两条 `Output`：先「正在压缩」，再收场行。**
    ///
    /// 时序是承重的：摘要是一次独立模型调用（可能数十秒），而 `capabilities::compact`
    /// 一进去就 `await`、中途没有出站口。若两条一起攒到最后发，用户在压缩全程看到的
    /// 是一个冻住的屏 —— 与 ADR-0052 修掉的「敲了没反应」同一种观感。
    ///
    /// 构造：一条**大**用户消息（≈50k 估算 token）让 `plan()` 切出非空摘要段。
    ///
    /// 变异：把开始行改成与 `lines` 一起发（或删掉它）⇒ 本用例的出站序列断言变红。
    #[tokio::test]
    async fn compact_emits_an_opening_line_before_the_result() {
        let model = MockModel::new("mock");
        model.push_text("hi");
        // 摘要调用消费这一条（顺序固定：先回合，再压缩）。
        model.push_text("HANDOFF SUMMARY");
        let mut h = spawn_app_loop(Box::new(model), true, Vec::new()).await;

        // 200k 个 ASCII 字符 ⇒ ≈50k 估算 token ⇒ 必超出保留预算（窗口的 10%），
        // 且远低于自动触发点（85%）⇒ 本回合不会被自动压缩干扰。
        h.requests.send(prompt(&"a".repeat(200_000))).await.unwrap();
        let _ = collect_until_view(&mut h).await;

        h.requests.send(Request::Compact).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);

        assert_eq!(msgs.len(), 2, "开始行 + 收场行，恰好两条：{msgs:?}");
        match &msgs[0] {
            Outbound::Output(line) => {
                assert!(
                    line.starts_with("Compacting context (~"),
                    "第一条必须是开始行：{line}"
                );
                assert!(
                    line.contains('~'),
                    "估算值必须带 ~ 前缀，不得暗示精确：{line}"
                );
            }
            other => panic!("第一条应为开始行：{other:?}"),
        }
        match &msgs[1] {
            Outbound::Output(line) => {
                assert!(
                    line.starts_with("Compacted: ~"),
                    "第二条必须是收场行：{line}"
                );
            }
            other => panic!("第二条应为收场行：{other:?}"),
        }
    }

    /// **压不了时不打开始行** —— 只有一行收场提示。
    ///
    /// 否则用户会看到「正在压缩…」紧跟「Nothing to compact.」这种自相矛盾的输出。
    ///
    /// 构造：新会话**一条消息都没有** ⇒ `plan()` 返回 `None`（`len < 2`）。
    /// 注意不能用「先跑一个小回合」来造 —— 两三条消息的会话**能**切出合法的
    /// 摘要/保留两段，`plan()` 会 `Some`，那时就真去调模型了。
    ///
    /// 变异：让 `app_loop` 无条件先发开始行（不认 `CompactOpening::Blocked`）
    /// ⇒ 出站序列变成两条 ⇒ 变红。
    #[tokio::test]
    async fn compact_that_cannot_run_emits_no_opening_line() {
        // 模型队列**空着**：若真去压，`MockModel` 的 `remove(0)` 会 panic。
        let mut h = spawn_app_loop(Box::new(MockModel::new("mock")), true, Vec::new()).await;

        h.requests.send(Request::Compact).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);

        assert_eq!(msgs.len(), 1, "压不了时只应有收场提示：{msgs:?}");
        match &msgs[0] {
            Outbound::Output(line) => {
                assert!(line.starts_with("Nothing to compact."), "{line}");
                assert!(!line.contains("Compacting"), "不得先说「正在压缩」：{line}");
            }
            other => panic!("应为收场提示：{other:?}"),
        }
    }

    /// **`/login` 的种子回落**：端点拿不到目录时，登录仍能定出模型与候选。
    ///
    /// harness 未接真端点，`fetch_models` 必然失败（网络/超时）⇒ 正好走到回落分支。
    /// 断言：不用凭证/目录也能登录成功；起始模型取种子第一条；目录进缓存 ⇒ 同一次
    /// 刷新的 `View` 里立刻有候选。
    ///
    /// 变异：去掉 `catalog.is_none()` 的种子回落 ⇒ `capabilities::login` 拿到 `None`，
    /// 只存凭证不切模型，两条断言均变红。
    #[tokio::test]
    async fn login_with_seed_catalog_skips_network_and_fills_candidates() {
        let mut h = spawn_app_loop(Box::new(MockModel::new("mock")), false, Vec::new()).await;

        let started = Instant::now();
        h.requests
            .send(Request::Login {
                provider: "glm".into(),
                api_key: "sk-glm".into(),
                // 不传 base：用该 provider 的内置 base（种子就按这个 base 预热）。
                api_base: None,
            })
            .await
            .unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let view = take_view(&mut msgs);
        let _ = started.elapsed();

        // 目录拉不到 ⇒ 回落种子；本次登录仍要能定出模型，而不是停在「只存凭证」。
        let [Outbound::Output(text)] = msgs.as_slice() else {
            panic!("登录应恰为 [Output] + View：{msgs:?}");
        };
        assert!(
            text.contains("Logged in to glm (glm-5.3)"),
            "起始模型取种子第一条：{text}"
        );
        assert_eq!(view.provider.as_deref(), Some("glm"));
        // 本 harness 未注册 model factory ⇒ `login` 无法重建 model，接线器仍持有
        // 启动时那个 mock。快照中的 `model` 只反映接线器（实际在跑的），故不断言它。
        assert_eq!(view.logged_in_providers, vec!["glm".to_string()]);
        // 目录进了缓存 ⇒ 同一次刷新的 `View` 里就有两条候选（不需要重启、不需要拉取）。
        for id in ["glm-5.3", "glm-5.3-flash"] {
            assert!(
                view.available_models.iter().any(|c| c.model == id),
                "登录后候选应含种子模型 {id}：{:?}",
                view.available_models
            );
        }
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
            backgrounds: _,
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

    /// **回合中的 `/compact` 在回合内执行**，不等回合结束。
    ///
    /// 这是本改动的端到端钉子：`Compact` 走**边界信道**（轮边界）而不是请求信道
    /// （只在回合之间消费）。用「回合 1 的回复把会话养大 + 回合 2 里投 Compact」
    /// 构造：若 `Compact` 仍走请求信道，它会排到回合 2 **结束之后**，本用例在
    /// 回合 2 的出站序列里就看不到 `ContextCompacted`。
    ///
    /// 变异：把 `drain_boundary` 的 `Compact` 臂删掉（只认 `Steer`）⇒ 不压缩 ⇒ 红。
    #[tokio::test]
    async fn compact_in_flight_runs_within_the_turn() {
        let model = MockModel::new("mock");
        // 回合 1：把历史养到远超触发点。
        model.push_text("a".repeat(28_000)); // ≈7000 token
        // 回合 2：摘要 + 正式回复（顺序取决于压缩在本轮的位置：Step 4b 自动压缩
        // 会先跑，但本用例窗口足够大、不自动触发 ⇒ 只有手动压缩一次）。
        model.push_text("SUMMARY");
        model.push_text("done");

        let mut h = spawn_app_loop(Box::new(model), false, Vec::new()).await;

        h.requests.send(prompt("hi")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);
        assert!(
            !has_compaction(&msgs),
            "回合 1 太小（默认窗口 128k，触发点 108k），不该压缩：{msgs:?}"
        );

        // 回合 2 进行中投 Compact —— 边界信道，下一轮边界执行。
        h.requests.send(prompt("hi")).await.unwrap();
        h.boundaries.send(Boundary::Compact).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);

        assert!(
            has_compaction(&msgs),
            "回合中的 Compact 必须在本回合内执行（轮边界），不得排到回合之后：{msgs:?}"
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
    /// 同时钉住 **`SetModel` 后立刻取到的快照**：effective 与 declared 都必须来自
    /// 新模型（`cfg`），否则切换瞬间会出现 effective > declared 的 `(capped)`。
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
        let view = take_view(&mut msgs);

        // **快照也必须是新模型的窗口**（`/status` 与 `/model` 同源）。
        // `SetModel` 只改 `cfg`，agent 的窗口要到下一回合开头才刷新；若快照的
        // effective 仍取 `agent.context_window()`，这里会是 128_000 / 8_000
        // —— effective > declared 却标 `(capped)`，与 `/model` 的输出自相矛盾。
        assert_eq!(
            view.context_window,
            Some(8_000),
            "SetModel 后快照的 effective 窗口必须取自新模型（不得停留在上一回合的 128k）"
        );
        assert_eq!(
            view.context_window_declared,
            Some(8_000),
            "声明窗口同样应取自新模型"
        );
        assert!(
            view.context_window.unwrap() <= view.context_window_declared.unwrap(),
            "effective 不得大于 declared（否则 /status 显示自相矛盾的 capped）：{:?} / {:?}",
            view.context_window,
            view.context_window_declared
        );

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
        let (skills, _) = crate::skills::scan(std::slice::from_ref(&root));
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

    /// **`Role::Harness` 的消息不展开**：正文即使长得完全像 skill 调用（`/foo`），也
    /// 必须**原样返回**、角色不得被改写成 `User`。
    ///
    /// skill 调用是**用户输入**的概念；harness 注入的输入（后台命令结果 / 占位消息）
    /// 不是用户在敲命令。展开路径会重建 `Message { role: Role::User, .. }` ⇒ 若不过滤
    /// 角色，后台结果会被当成用户发言（落盘 / 回放 / 统计全部说错来源）。
    ///
    /// 变异：删掉 `expand_skill_invocation` 开头的 `if message.role != Role::User`
    /// ⇒ 本用例变红（正文被换成信封、角色变 `User`）。
    #[test]
    fn harness_role_is_not_rewritten_by_skill_expansion() {
        let root = temp_dir_for("expand_harness");
        let dir = root.join("foo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\ndescription: 演示\n---\n正文\n").unwrap();
        let (skills, _) = crate::skills::scan(std::slice::from_ref(&root));
        assert_eq!(skills.len(), 1, "测试前提：恰好扫到一条 skill");
        // 同一个调用形式用 `User` 是会展开的 —— 否则下面的断言就分不出「角色门控」
        // 与「这个调用本来就没命中」。
        let as_user = user_message("/foo");
        assert_ne!(
            expand_skill_invocation(as_user.clone(), &skills),
            as_user,
            "测试前提：`/foo` 在 `Role::User` 下确实会被展开"
        );

        let harness = Message {
            role: Role::Harness,
            content: vec![ContentBlock::Text {
                text: "/foo".to_string(),
            }],
        };
        let out = expand_skill_invocation(harness.clone(), &skills);
        assert_eq!(out, harness, "harness 输入必须逐字节原样返回");
        assert_eq!(out.role, Role::Harness, "角色不得被改写成 `User`");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 建一份临时 skill 目录并扫描成表（恰好一条 `demo`）。
    fn demo_skill_table(tag: &str) -> (PathBuf, Vec<crate::skills::SkillEntry>) {
        let root = temp_dir_for(tag);
        let dir = root.join("demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\ndescription: 演示技能\n---\n正文第一行\n正文第二行\n",
        )
        .unwrap();
        let (skills, _) = crate::skills::scan(std::slice::from_ref(&root));
        assert_eq!(skills.len(), 1, "测试前提：恰好扫到一条 skill");
        (root, skills)
    }

    /// **展开只换首个 `Text` 块，图片块原位保留**。
    ///
    /// 用户发「`/demo` + 一张截图」时，`content` 是 `[Text("/demo"), Image]`；
    /// 展开若把消息重建成「只有一个 `Text` 块」，图片就会**静默消失**（无编译错误、
    /// 无运行时告警）。本用例逐字段把图片块钉住。
    ///
    /// 变异：展开时写回 `content: vec![ContentBlock::Text { text: expanded }]`
    /// ⇒ 本用例变红（`content.len()` 变 1，图片断言失败）。
    #[test]
    fn skill_expansion_keeps_image_block() {
        let (root, skills) = demo_skill_table("expand_image");
        let image = ContentBlock::Image {
            path: "/tmp/shot.png".into(),
            detail: Some(ys_core::ImageDetail::High),
        };
        let msg = Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "/demo 帮我看看".into(),
                },
                image.clone(),
            ],
        };

        let out = expand_skill_invocation(msg, &skills);
        assert_eq!(out.role, Role::User, "展开后仍是用户发言");
        assert_eq!(out.content.len(), 2, "块数不得变化：{:?}", out.content);
        match &out.content[0] {
            ContentBlock::Text { text } => {
                assert!(
                    text.contains("<skill name=\"demo\""),
                    "首块应为展开信封：{text}"
                );
                assert!(text.contains("正文第一行"), "信封里要有 skill 正文：{text}");
                assert!(text.contains("帮我看看"), "参数要在信封尾部：{text}");
            }
            other => panic!("首位应仍是 `Text` 块，实为 {other:?}"),
        }
        assert_eq!(out.content[1], image, "图片块必须逐字段原样保留");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// **不回归**：不命中的普通消息逐字段不变（既不套信封、也不动块结构）。
    #[test]
    fn plain_message_is_untouched_by_expansion() {
        let (root, skills) = demo_skill_table("expand_plain");
        let msg = user_message("普通消息，不是命令");
        let out = expand_skill_invocation(msg.clone(), &skills);
        assert_eq!(out, msg, "不命中的消息必须逐字段不变");
        assert_eq!(out.content.len(), 1, "块数不得变化");

        // 未知命令同样不变。
        let unknown = user_message("/nope 参数");
        assert_eq!(expand_skill_invocation(unknown.clone(), &skills), unknown);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// **多块保序**：`[Text(/demo), Image, Text(尾巴)]` ⇒ 只有首个 `Text` 被替换，
    /// 图片与**第二个 `Text` 都还在各自的下标**上（不得被吞、不得被前移）。
    ///
    /// 变异：展开时只保留首个 `Text`（或把展开结果 unshift 到最前）⇒ 本用例变红。
    #[test]
    fn skill_expansion_preserves_block_order() {
        let (root, skills) = demo_skill_table("expand_order");
        let image = ContentBlock::Image {
            path: "/tmp/a.png".into(),
            detail: None,
        };
        let tail = ContentBlock::Text {
            text: "尾巴".into(),
        };
        let msg = Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "/demo".into(),
                },
                image.clone(),
                tail.clone(),
            ],
        };

        let out = expand_skill_invocation(msg, &skills);
        assert_eq!(out.content.len(), 3, "块数不得变化：{:?}", out.content);
        match &out.content[0] {
            ContentBlock::Text { text } => {
                assert!(
                    text.contains("<skill name=\"demo\""),
                    "首块应为展开信封：{text}"
                )
            }
            other => panic!("首位应仍是 `Text` 块，实为 {other:?}"),
        }
        assert_eq!(out.content[1], image, "图片必须留在下标 1");
        assert_eq!(
            out.content[2], tail,
            "第二个 `Text` 必须原样留在下标 2（不得被替换成展开结果）"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// **首块非 `Text` ⇒ 整条不展开**（保持既有语义：只认纯文本首块的 `/` 命名空间）。
    ///
    /// 放宽到「在任意位置找首个 `Text`」会改变既有语义，不在本任务范围。这里把
    /// 契约钉住：`[Image, Text("/demo")]` 原样返回，图片仍在首位。
    #[test]
    fn non_text_first_block_is_not_expanded() {
        let (root, skills) = demo_skill_table("expand_nontext");
        // 前置：同样的调用在纯文本下确实会展开，否则本用例分不出「首块门控」与
        // 「这个命令本来就没命中」。
        assert_ne!(
            expand_skill_invocation(user_message("/demo"), &skills),
            user_message("/demo"),
            "测试前提：纯文本 `/demo` 会展开"
        );

        let image_first = Message {
            role: Role::User,
            content: vec![
                ContentBlock::Image {
                    path: "/tmp/first.png".into(),
                    detail: Some(ys_core::ImageDetail::Low),
                },
                ContentBlock::Text {
                    text: "/demo".into(),
                },
            ],
        };
        let out = expand_skill_invocation(image_first.clone(), &skills);
        assert_eq!(out, image_first, "首块非 `Text` ⇒ 整条逐字段不变");
        assert!(
            matches!(out.content.first(), Some(ContentBlock::Image { .. })),
            "图片仍在首位：{:?}",
            out.content
        );

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

    /// 子 agent 在**回合内**完成时，结果必须在**本回合**就送达（不能白等一个回合）。
    ///
    /// 这条钉的是「落点绑定」这一半：投递表是进程级的，若本回合没绑定落点，
    /// 结果就只能留表、等下一回合补投 —— 功能上「不丢」，但模型多等了一整回合。
    ///
    /// 变异：`let _subagent_binding = ...` 改成 `let _ = ...`（守卫立即 drop）
    /// ⇒ 本用例变红；另一个用例（跨回合）仍绿 —— 两条各钉一半。
    #[tokio::test]
    async fn async_subagent_result_delivered_within_the_same_turn() {
        const CHILD_PROMPT: &str = "你是子 agent";
        const CHILD_OUTPUT: &str = "CHILD-SAME-TURN";

        struct Inner {
            parent_calls: AtomicU32,
            requests: Mutex<Vec<ModelRequest>>,
        }
        struct SameTurnModel(Arc<Inner>);

        #[async_trait::async_trait]
        impl Model for SameTurnModel {
            fn model_id(&self) -> &str {
                "same-turn"
            }

            async fn complete(
                &self,
                request: ModelRequest,
                _sink: &mut dyn ModelEventSink,
            ) -> Result<ModelResponse, ModelError> {
                // 子 agent **立刻**返回 ⇒ 结果在父回合还活着时就已就绪。
                if request.system.as_deref() == Some(CHILD_PROMPT) {
                    return Ok(text_response(CHILD_OUTPUT));
                }
                self.0.requests.lock().unwrap().push(request);
                let n = self.0.parent_calls.fetch_add(1, Ordering::Relaxed);
                if n == 0 {
                    Ok(ModelResponse {
                        message: Message {
                            role: Role::Assistant,
                            content: vec![ContentBlock::ToolUse {
                                id: ToolCallId("call-1".into()),
                                name: "subagent_async".into(),
                                arguments: serde_json::json!({
                                    "description": "快活",
                                    "prompt": "干快活",
                                }),
                            }],
                        },
                        usage: Usage::default(),
                        finish_reason: FinishReason::ToolCalls,
                    })
                } else if n < 6 {
                    // 父继续做自己的事：给子 agent 足够时间在同一回合内完成。
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    Ok(ModelResponse {
                        message: Message {
                            role: Role::Assistant,
                            content: vec![ContentBlock::ToolUse {
                                id: ToolCallId(format!("call-{n}")),
                                name: "no_such_tool".into(),
                                arguments: serde_json::Value::Null,
                            }],
                        },
                        usage: Usage::default(),
                        finish_reason: FinishReason::ToolCalls,
                    })
                } else {
                    Ok(text_response("parent done"))
                }
            }
        }

        let inner = Arc::new(Inner {
            parent_calls: AtomicU32::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let mut h = spawn_app_loop_full(
            Box::new(SameTurnModel(Arc::clone(&inner))),
            false,
            Vec::new(),
            |_, _| {},
            |b| b.tools(vec![Box::new(crate::subagent::SubagentAsyncTool::new())]),
            |tmp| {
                Some(crate::subagent::SubagentSetup::shared(
                    CHILD_PROMPT.to_string(),
                    ys_component::RunLimits::new(3),
                    Duration::from_secs(30),
                    tmp.to_path_buf(),
                    WebConfig::offline(FetchedDir::Temp(tmp.join("fetched"))),
                    Arc::new(crate::subagent::SubagentsDir::at(
                        tmp.join("subagents"),
                        false,
                    )),
                    Arc::new(crate::subagent::PendingResults::default()),
                    Arc::new(crate::subagent::LiveSubagents::default()),
                ))
            },
            None,
            false,
            false,
            false,
        )
        .await;

        // **只跑一个回合** —— 结果必须在这一个回合内进会话。
        h.requests.send(prompt("hi")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);

        let requests = inner.requests.lock().unwrap().clone();
        assert!(
            requests.iter().any(|r| has_text(r, CHILD_OUTPUT)),
            "子 agent 在回合内完成时，结果必须在**本回合**的后续轮次送达 \
             （否则白等一整个回合）；请求数 = {}",
            requests.len()
        );
        // 并且确实经 `DeferredResult` 事件（而非伪装成用户消息）。
        let deferred = events_of(&msgs)
            .iter()
            .filter(|e| matches!(e.event, AgentEvent::DeferredResult { .. }))
            .count();
        assert_eq!(deferred, 1, "应恰好一条 DeferredResult 事件");
    }

    /// **契约已变（ADR-0055）**：本用例现在钉的是「**结果一旦在表里，就不会丢**」
    /// 这条兼底保证 —— 而不是第一版设想的「回合结束后完成」那条时序。
    ///
    /// 因此本用例现在验证的事实是：
    /// ① 子 agent **没有被取消**（`tokio::spawn` 分离任务，回合结束不等于它死）；
    /// ② 即使走跨回合路径，结果也**不会丢**（投递表 + 回合开头补投）；
    /// ③ 第二回合的请求里能看到它。
    ///
    /// 生产证据（会话 `1792840206_799154000`，修复前的故障）：turn 11 于 20:13:36 派发
    /// 3 个 `subagent_async`，回合 20:14:28 结束；子 agent 分别于 20:16:59 / 20:20:47
    /// 完成并落盘转录，但结果**从未**进入任何后续 `ModelRequest` —— 模型只能反复回答
    /// 「还在跑」。这正是 ADR-0055 要消除的形态。
    ///
    /// 变异：把 `deliver` 的未绑定分支改成「直接丢弃」⇒ 第二回合的 `has_text` 断言变红。
    #[tokio::test]
    async fn async_subagent_result_survives_turn_boundary() {
        /// 子 agent 的 system prompt —— 本用例用它区分父 / 子调用。
        const CHILD_PROMPT: &str = "你是子 agent";
        /// 子 agent 的产出正文 —— 断言只认这个标记。
        const CHILD_OUTPUT: &str = "CHILD-LATE-OUTPUT";

        struct Inner {
            parent_calls: AtomicU32,
            child_calls: AtomicU32,
            requests: Mutex<Vec<ModelRequest>>,
        }
        struct AsyncLateModel(Arc<Inner>);

        #[async_trait::async_trait]
        impl Model for AsyncLateModel {
            fn model_id(&self) -> &str {
                "async-late"
            }

            async fn complete(
                &self,
                request: ModelRequest,
                _sink: &mut dyn ModelEventSink,
            ) -> Result<ModelResponse, ModelError> {
                // 子 agent：慢于整个父回合 ⇒ 它必定在回合结束之后才完成。
                if request.system.as_deref() == Some(CHILD_PROMPT) {
                    self.0.child_calls.fetch_add(1, Ordering::Relaxed);
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    return Ok(text_response(CHILD_OUTPUT));
                }
                self.0.requests.lock().unwrap().push(request);
                let n = self.0.parent_calls.fetch_add(1, Ordering::Relaxed);
                if n == 0 {
                    // 父第 1 轮：派发后台子 agent。
                    Ok(ModelResponse {
                        message: Message {
                            role: Role::Assistant,
                            content: vec![ContentBlock::ToolUse {
                                id: ToolCallId("call-1".into()),
                                name: "subagent_async".into(),
                                arguments: serde_json::json!({
                                    "description": "慢活",
                                    "prompt": "干慢活",
                                }),
                            }],
                        },
                        usage: Usage::default(),
                        finish_reason: FinishReason::ToolCalls,
                    })
                } else {
                    // 父第 2 轮起：立刻收尾（此刻子 agent 还在睡）。
                    Ok(text_response("parent done"))
                }
            }
        }

        let inner = Arc::new(Inner {
            parent_calls: AtomicU32::new(0),
            child_calls: AtomicU32::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let mut h = spawn_app_loop_full(
            Box::new(AsyncLateModel(Arc::clone(&inner))),
            false,
            Vec::new(),
            |_, _| {},
            |b| b.tools(vec![Box::new(crate::subagent::SubagentAsyncTool::new())]),
            |tmp| {
                Some(crate::subagent::SubagentSetup::shared(
                    CHILD_PROMPT.to_string(),
                    ys_component::RunLimits::new(3),
                    Duration::from_secs(30),
                    tmp.to_path_buf(),
                    WebConfig::offline(FetchedDir::Temp(tmp.join("fetched"))),
                    Arc::new(crate::subagent::SubagentsDir::at(
                        tmp.join("subagents"),
                        false,
                    )),
                    Arc::new(crate::subagent::PendingResults::default()),
                    Arc::new(crate::subagent::LiveSubagents::default()),
                ))
            },
            None,
            false,
            false,
            false,
        )
        .await;

        // 回合 1：派发后立刻收尾（子 agent 仍在后台跑）。
        h.requests.send(prompt("hi")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);

        // 等子 agent 真正跑完（300ms 睡眠 + 余量）—— 此后它的结果必须已在
        // 某个**跨回合**的落点上，而不是随回合 1 的边界源一起销毁。
        tokio::time::sleep(Duration::from_millis(700)).await;
        // 结果落地会**自动起一个回合**（`Wake::SubagentReturned`）—— 先把那一回合的
        // 收尾 `View` 收掉，否则它会被误当成下面那个回合的收尾。
        let mut auto = collect_until_view(&mut h).await;
        let _ = take_view(&mut auto);

        // 回合 2：模型必须能在请求里看到子 agent 的产出。
        h.requests.send(prompt("结果呢")).await.unwrap();
        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);

        let requests = inner.requests.lock().unwrap().clone();
        // 区分性事实：子 agent **确实跑了**（不是被取消）—— 它的产出只在 write
        // transcript 之外无处可去，故「取消」这一承诺与实现不符。
        assert!(
            inner.child_calls.load(Ordering::Relaxed) >= 1,
            "子 agent 必须在回合结束后仍继续运行（tokio::spawn 分离任务）—— \
             若此处为 0，说明它被取消了，与生产日志（转录在回合结束后 2~6 分钟落盘）矛盾"
        );
        assert!(
            requests.len() >= 3,
            "父回合应至少发生 3 次 model 调用（派发 / 收尾 / 第二回合），实际 {}",
            requests.len()
        );
        let turn2 = requests.last().unwrap();
        assert!(
            has_text(turn2, CHILD_OUTPUT),
            "子 agent 在回合结束后完成时，结果必须进入下一回合的 ModelRequest \
             （否则「自动送达」失效，模型只会反复说「还在跑」）。\
             第二回合请求里没有 {CHILD_OUTPUT}；消息数 = {}",
            turn2.messages.len()
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

    /// 同 [`spawn_resumable`]，但把工作目录建在一个**真实 git 仓库**里。
    ///
    /// 这些用例要的是「有循环在跑时换会话」这个前提，仓库只是为了让工作区形态与
    /// 真实场景一致（定时循环本身不探工作区形态，普通目录同样能跑）。
    async fn spawn_resumable_in_git(replies: usize, tag: &str) -> Harness {
        let tmp = TempDir::new(tag);
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo);
        // `tmp` 的 `Drop` 会删掉 repo ⇒ 本用例要求它在整个用例期间存在，
        // 而 `Harness` 持有的是它**自己**的临时目录，装不下这个。
        // 故显式泄漏：测试进程短命，路径带 pid + 原子序号，不会串台。
        std::mem::forget(tmp);
        let initial = MockModel::new("initial");
        initial.push_text("initial");
        spawn_app_loop_with(Box::new(initial), true, Vec::new(), move |config, _tmp| {
            config.cwd = repo;
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

        fn set_disabled(log_dir: &Path) -> Self {
            let guard = crate::test_env::env_lock();
            let restore = crate::test_env::EnvRestore::capture(&[
                crate::logging::LOG_DIR_ENV,
                crate::debug_log::DEBUG_ENV,
            ]);
            // SAFETY: 持有 env_lock。
            unsafe {
                std::env::set_var(crate::logging::LOG_DIR_ENV, log_dir);
                std::env::set_var(crate::debug_log::DEBUG_ENV, "0");
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
        let msgs = collect_until_view(&mut h).await;
        shutdown(h).await;

        // 读数**不进 transcript**：TUI 用户看不到 `turn=…` 这一类运行读数。
        let output = output_text(&msgs);
        assert!(
            !output.contains("turn=1 rounds=") && !output.contains("tpr_x100="),
            "TUI transcript 不得出现读数行：{output}"
        );

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
            "run_reading",
            "run_end",
        ] {
            assert!(evs.contains(&expected), "缺少 `{expected}` 记录：{evs:?}");
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
        assert_eq!(
            evs.iter().filter(|ev| **ev == "run_reading").count(),
            1,
            "一个回合恰一条 `run_reading`"
        );

        // 既有文本行形状不变（逐字节口径由 `round_clock` 的单测钉住）。
        let text_log = std::fs::read_to_string(logs.0.join("yushan.log")).unwrap();
        assert!(
            text_log.lines().any(|line| line.contains(" round=1 ")),
            "逐轮文本行应照旧：{text_log}"
        );
        assert!(
            text_log
                .lines()
                .any(|line| line.contains(" turn=1 rounds=")),
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
            false,
        )
        .await;

        h.requests.send(prompt("hi")).await.unwrap();
        let _ = collect_until_view(&mut h).await;
        let session = h.session_path.get().expect("持久会话应有路径");
        let id = session.file_stem().unwrap().to_string_lossy().into_owned();

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
        assert_eq!(
            runs.len(),
            2,
            "启动 + `/resume` 各一条 `run_start`：{runs:?}"
        );
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
        let _env = DebugEnv::set_disabled(&logs.0);

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
            false,
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

    // ───────────── 后台命令的清理点（设计 §4.5 的六个清理点之二） ─────────────

    /// 后台用例的**环境隔离**：整段持有 `env_lock`，并把 `YUSHAN_DEBUG` 关掉、
    /// `YUSHAN_LOG_DIR` 指到临时目录。
    ///
    /// **为什么必须整段持有**：后台命令的启动（`sh -c …`）与杀进程组
    /// （`sh -c "kill -9 -<pgid>"`）都**按 `PATH` 解析 `sh`** ⇒ 别的用例把
    /// `PATH` 指向空目录的窗口里（`toolset.rs` 的 `set_path`），`sh` 找不到 ⇒
    /// 启动与清理**双双静默失败**（`kill_group` 的失败是静默的）。
    /// 读 `PATH` 也要与改写串行 ⇒ 持锁。
    ///
    /// 字段顺序即 drop 顺序：先恢复环境（此时锁仍活着），再释放锁。
    struct BgEnv {
        _restore: crate::test_env::EnvRestore,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl BgEnv {
        fn set(log_dir: &Path) -> Self {
            let guard = crate::test_env::env_lock();
            let restore = crate::test_env::EnvRestore::capture(&[
                crate::logging::LOG_DIR_ENV,
                crate::debug_log::DEBUG_ENV,
            ]);
            // SAFETY: 持有 env_lock。
            unsafe {
                std::env::set_var(crate::logging::LOG_DIR_ENV, log_dir);
                std::env::set_var(crate::debug_log::DEBUG_ENV, "0");
            }
            Self {
                _restore: restore,
                _guard: guard,
            }
        }
    }

    /// 用例级收尾：结束时（**含 panic 展开**）杀掉本用例起的后台命令并**收尸**。
    ///
    /// 断言用的是**真进程** ⇒ 用例失败时若不收尾就会留下孤儿；被 SIGKILL 的
    /// 进程还要**收尸**才从进程表消失（僵尸仍占着 pid，`pgrep` 照样看得见）。
    struct BgCleanup {
        tasks: Arc<BackgroundTasks>,
        sessions: Vec<String>,
        pids: Vec<u32>,
    }

    impl BgCleanup {
        fn new(tasks: &Arc<BackgroundTasks>) -> Self {
            Self {
                tasks: Arc::clone(tasks),
                sessions: Vec::new(),
                pids: Vec::new(),
            }
        }

        /// 起一条**真实**的后台命令（`sleep 300`）—— 返回 `(句柄, pid)`。
        ///
        /// 用真进程（而不是替身）才能断言「进程存在性」—— 替身只能证明
        /// 「我们记的账变了」，证明不了「进程真的死了」。
        fn start(&mut self, session: &str) -> (ys_tools_basic::BgHandle, u32) {
            self.start_with(session, "sleep 300")
        }

        /// 起一条**真实**的后台命令（命令可定制）—— 返回 `(句柄, pid)`。
        fn start_with(&mut self, session: &str, command: &str) -> (ys_tools_basic::BgHandle, u32) {
            let handle = self
                .tasks
                .start(session, command, &std::env::temp_dir(), None)
                .expect("启动后台命令");
            let pid = self
                .tasks
                .snapshot(handle, session)
                .expect("刚启动的条目应有快照")
                .pid;
            self.track_session(session);
            self.pids.push(pid);
            (handle, pid)
        }

        /// 把一条**外部**启动的会话也纳入收尾（不经本结构体的 `start*` 起命令时用）。
        fn track_session(&mut self, session: &str) {
            if !self.sessions.iter().any(|known| known == session) {
                self.sessions.push(session.to_string());
            }
        }
    }

    impl Drop for BgCleanup {
        fn drop(&mut self) {
            for session in &self.sessions {
                self.tasks.kill_all(session);
            }
            for _ in 0..100 {
                self.tasks.sweep();
                if self.pids.iter().all(|pid| !process_alive(*pid)) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    /// 探活：进程**是否存在**（POSIX `kill -0` 语义）。
    ///
    /// 问内核，不问注册表 —— 注册表的状态是「我们自己记的账」。
    fn process_alive(pid: u32) -> bool {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("kill -0 {pid} 2>/dev/null"))
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    /// 等某 pid 的进程**从进程表里消失**（最多 5s）。
    ///
    /// 被 SIGKILL 的进程要先被**收尸**才从进程表消失（僵尸仍占着 pid，
    /// `kill -0` 对它照样成功）⇒ 每轮先让注册表收尸再探活。
    async fn wait_process_gone(tasks: &BackgroundTasks, pid: u32) {
        for _ in 0..500 {
            tasks.sweep();
            if !process_alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("进程 {pid} 仍在：清理点没有把它杀掉");
    }

    /// **清理点之一（abort）**：UI 中止 ⇒ 本会话的后台命令进程组被杀。
    ///
    /// 断言**进程存在性**（不看工具文案）—— 进程表里没有它才算数。
    ///
    /// 变异：把 abort 臂里的 `kill_all` 删掉 ⇒ 进程仍在 ⇒ 本用例变红。
    #[tokio::test]
    async fn abort_kills_the_background_processes_of_the_current_session() {
        let logs = TempDir::new("bg_logs");
        let _env = BgEnv::set(&logs.0);
        let (model, mut probe) = probe_model(ProbePlan::AlwaysToolCall);
        let mut h = spawn_app_loop_backgrounds(Box::new(model), true).await;
        let (tasks, slot) = h
            .backgrounds
            .as_ref()
            .expect("本用例挂了后台上下文")
            .clone();
        let session = slot.get();
        let mut cleanup = BgCleanup::new(&tasks);
        let (_handle, pid) = cleanup.start(&session);

        h.requests.send(prompt("hi")).await.unwrap();
        probe.wait_entered(1).await;
        h.boundaries.send(Boundary::Abort).await.unwrap();
        probe.let_app_task_pump().await;
        probe.release();
        let _ = collect_until_view(&mut h).await;

        wait_process_gone(&tasks, pid).await;
    }

    /// **清理点之二（回合预算到点）**：wall-clock 预算耗尽 ⇒ 与 abort 同一语义。
    ///
    /// 变异：把预算臂里的 `kill_all` 删掉 ⇒ 进程仍在 ⇒ 本用例变红。
    #[tokio::test]
    async fn budget_exhaustion_kills_the_background_processes_of_the_current_session() {
        let logs = TempDir::new("bg_logs");
        let _env = BgEnv::set(&logs.0);
        let (model, mut probe) = probe_model(ProbePlan::AlwaysToolCall);
        let mut h = spawn_app_loop_backgrounds_with(Box::new(model), true, |config, tmp| {
            let path = tmp.join("config.json");
            std::fs::write(&path, r#"{"max_turn_duration_secs": 1}"#).unwrap();
            config.load_user_config(&path);
        })
        .await;
        let (tasks, slot) = h
            .backgrounds
            .as_ref()
            .expect("本用例挂了后台上下文")
            .clone();
        let session = slot.get();
        let mut cleanup = BgCleanup::new(&tasks);
        let (_handle, pid) = cleanup.start(&session);

        h.requests.send(prompt("hi")).await.unwrap();
        probe.wait_entered(1).await;

        // 等预算到点：app 在回合**仍进行中**发说明行（计时器与回合并发）。
        let mut msgs = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(8), async {
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
        probe.let_app_task_pump().await;
        probe.release();
        msgs.extend(collect_until_view(&mut h).await);

        wait_process_gone(&tasks, pid).await;
    }

    /// **清理点之三（`/new`）**：换会话 ⇒ 旧会话的后台命令被杀 **+ 旧会话的
    /// 输出目录被删**。
    ///
    /// 变异：删掉 `/new` 臂里的 `discard_session_backgrounds` ⇒ 进程仍在、
    /// 目录也还在 ⇒ 本用例变红。
    #[tokio::test]
    async fn new_session_kills_background_processes_and_removes_the_output_directory() {
        let logs = TempDir::new("bg_logs");
        let _env = BgEnv::set(&logs.0);
        let model = MockModel::new("mock");
        model.push_text("hi");
        let mut h = spawn_app_loop_backgrounds(Box::new(model), true).await;
        let (tasks, slot) = h
            .backgrounds
            .as_ref()
            .expect("本用例挂了后台上下文")
            .clone();
        let session = slot.get();
        let mut cleanup = BgCleanup::new(&tasks);
        let (_handle, pid) = cleanup.start(&session);
        let session_dir = tasks.root().join(&session);
        assert!(
            session_dir.is_dir(),
            "后台命令的会话目录应已建好：{}",
            session_dir.display()
        );

        h.requests.send(Request::NewSession).await.unwrap();
        let _ = collect_until_view(&mut h).await;

        wait_process_gone(&tasks, pid).await;
        assert!(
            !session_dir.exists(),
            "/new 后旧会话的输出目录应被删除：{}",
            session_dir.display()
        );
        assert_ne!(
            slot.get(),
            session,
            "/new 后会话槽应指向新会话（旧 id 只会打空清理）"
        );
    }

    /// **清理点之四（`/resume`）**：切到别的会话 ⇒ 旧会话的后台命令被杀。
    ///
    /// 变异：删掉 `/resume` 臂里的 `kill_background_session` ⇒ 进程仍在 ⇒
    /// 本用例变红。
    #[tokio::test]
    async fn resume_session_kills_the_old_session_background_processes() {
        let logs = TempDir::new("bg_logs");
        let _env = BgEnv::set(&logs.0);
        let mut h = spawn_app_loop_backgrounds_with(
            Box::new(MockModel::new("mock")),
            true,
            |config, _tmp| {
                config.api_base = Some("http://resume-base/v1".into());
                config.api_key = Some("sk-resume".into());
                config.set_model_factory(move |_| Some(Box::new(MockModel::new("resumed"))));
            },
        )
        .await;
        let (tasks, slot) = h
            .backgrounds
            .as_ref()
            .expect("本用例挂了后台上下文")
            .clone();
        let session = slot.get();
        let mut cleanup = BgCleanup::new(&tasks);
        let (_handle, pid) = cleanup.start(&session);

        let mut meta = ys_session::SessionMeta::new(0);
        meta.title = Some("目标会话".into());
        write_target_session(&h, "target-1", &meta, &[user_message("hello")]);

        h.requests
            .send(Request::ResumeSession {
                target: "target-1".into(),
            })
            .await
            .unwrap();
        let _ = collect_until_view(&mut h).await;

        wait_process_gone(&tasks, pid).await;
        assert_ne!(slot.get(), session, "/resume 后会话槽应指向目标会话");
    }

    /// **按会话过滤**：清理本会话 ⇒ **别的会话**的后台命令不受影响。
    ///
    /// 这条是「清理点按会话过滤」的全部意义（设计 §4.5）：漏掉的清理点、
    /// 新增的清理路径都不得波及别的会话。
    ///
    /// 变异：把 `kill_all` 的会话过滤去掉（改成全局清空）⇒ 另一个会话的
    /// 进程也被杀 ⇒ 本用例变红。
    #[tokio::test]
    async fn cleaning_one_session_does_not_kill_another_session() {
        let logs = TempDir::new("bg_logs");
        let _env = BgEnv::set(&logs.0);
        let (model, mut probe) = probe_model(ProbePlan::AlwaysToolCall);
        let mut h = spawn_app_loop_backgrounds(Box::new(model), true).await;
        let (tasks, slot) = h
            .backgrounds
            .as_ref()
            .expect("本用例挂了后台上下文")
            .clone();
        let session = slot.get();
        let mut cleanup = BgCleanup::new(&tasks);
        let (_own, own_pid) = cleanup.start(&session);
        // 另一个会话的条目：注册表按会话过滤 ⇒ 清理本会话不该碰它。
        let (_other, other_pid) = cleanup.start("other-session");

        h.requests.send(prompt("hi")).await.unwrap();
        probe.wait_entered(1).await;
        h.boundaries.send(Boundary::Abort).await.unwrap();
        probe.let_app_task_pump().await;
        probe.release();
        let _ = collect_until_view(&mut h).await;

        wait_process_gone(&tasks, own_pid).await;
        assert!(
            process_alive(other_pid),
            "别的会话的后台命令不该被误杀（清理必须按会话过滤）"
        );
    }

    /// **共享 sweeper 在跑**：它把已退出的子进程**收尸** —— 不经任何显式
    /// `sweep()` 调用，进程也从进程表里消失（否则它一直是个僵尸：僵尸仍占着
    /// pid，`kill -0` 照样成功）。
    ///
    /// 变异：删掉 `run` 里的 `ensure_background_sweeper` ⇒ 没人收尸 ⇒
    /// 进程一直是僵尸 ⇒ 本用例变红。
    #[tokio::test]
    async fn the_shared_sweeper_reaps_finished_commands() {
        let logs = TempDir::new("bg_logs");
        let _env = BgEnv::set(&logs.0);
        let model = MockModel::new("mock");
        let h = spawn_app_loop_backgrounds(Box::new(model), true).await;
        let (tasks, slot) = h
            .backgrounds
            .as_ref()
            .expect("本用例挂了后台上下文")
            .clone();
        let session = slot.get();
        let mut cleanup = BgCleanup::new(&tasks);
        // 命令自己跑完 ⇒ 之后只剩「僵尸」与「被收尸」两种状态。
        let (_handle, pid) = cleanup.start_with(&session, "sleep 0.3");

        let mut reaped = false;
        for _ in 0..100 {
            if !process_alive(pid) {
                reaped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            reaped,
            "已退出的后台命令应被共享 sweeper 收尸（否则它一直占着 pid）"
        );
    }

    /// **后台结果在空闲态自动起一轮**，并把结果带进上下文。
    ///
    /// 走真实链路：`echo` 命令自己退出 ⇒ 共享 sweeper 的 `sweep` 推入终态 ⇒ 锁外
    /// `notify` ⇒ app 循环的 `Wake::BackgroundReturned` 臂 ⇒ 复核通过 ⇒ 起一个回合。
    ///
    /// 三重断言（缺一都不足以钉住行为）：
    /// ① 模型**多了一次调用**（回合真起了，而用户只提交了那一条请求）；
    /// ② 该回合里确有命令的完成结果（`DeferredResult`，正文来自真实工具输出）；
    /// ③ 占位消息的角色是 `Harness`，不是 `User`（它是 harness 输入，不是用户发言）。
    ///
    /// 变异：把该臂改回「只发一行 `Output` 提示、`continue`」⇒ ① 变红；
    /// 去掉 `expand_skill_invocation` 的角色门控 ⇒ ③ 变红。
    #[tokio::test]
    async fn late_background_result_starts_a_turn_while_idle() {
        const MARKER: &str = "BG-ONE-SHOT-MARKER";

        let logs = TempDir::new("bg_wake_logs");
        let _env = BgEnv::set(&logs.0);
        let calls = Arc::new(CallLog::default());
        let mut h =
            spawn_app_loop_backgrounds(Box::new(CountingModel(Arc::clone(&calls))), true).await;
        let (tasks, slot) = h
            .backgrounds
            .as_ref()
            .expect("本用例挂了后台上下文")
            .clone();
        let session = slot.get();
        let mut cleanup = BgCleanup::new(&tasks);
        // marker 只进**工具输出**（渲染成 `Deferred` 正文）；占位消息里没有它。
        let (_handle, _pid) = cleanup.start_with(&session, &format!("echo {MARKER} >&2"));

        // 回合 1：本用例唯一的一条用户请求。
        h.requests.send(prompt("起个后台命令")).await.unwrap();
        let first = collect_until_view(&mut h).await;
        assert_eq!(calls.count(), 1, "前提：第 1 个回合恰好一次模型调用");

        // **不再发任何请求** —— 空闲态必须自己起一轮。
        let (background_turn, text) =
            wait_for_one_turn_with(&mut h, first, &[MARKER], Duration::from_secs(10)).await;
        assert!(
            background_turn >= 2,
            "完成结果必须落进**自动起的那个回合**（第 1 回合只是起命令）：{background_turn}"
        );
        assert!(text.contains(MARKER), "结果正文必须带上命令输出：{text}");

        assert!(
            calls.count() >= 2,
            "空闲态必须再起一个回合（模型调用数应从 1 增至 ≥2），实际 {}",
            calls.count()
        );
        let last = calls.requests().pop().expect("应有模型调用记录");
        assert!(
            has_text(&last, MARKER),
            "自动起的回合必须已把命令结果注入模型上下文：{last:?}"
        );
        assert!(
            last.messages
                .iter()
                .any(|m| m.role == Role::Harness && has_text_in(m, "[harness]")),
            "占位输入必须是 `Role::Harness`（harness 产生的输入，非用户发言）：{last:?}"
        );
        assert!(
            !last
                .messages
                .iter()
                .any(|m| m.role == Role::User && has_text_in(m, "[harness]")),
            "占位输入不得落成用户消息（`expand_skill_invocation` 不得改角色）：{last:?}"
        );
    }

    /// **一次取完**：三条命令的结果必须落进**同一个**回合的上下文。
    ///
    /// 钉的是原缺陷：限流 2 条时「第三条留在 `Pending`、而唤醒只在**新终态**时发」
    /// 会变成死角 —— 再也没人唤醒它，用户得再发言两次才收得全。
    ///
    /// 三条命令都是 `sleep 0.2; echo …`：退出时刻相差几毫秒 ⇒ 同一个 100ms tick 里
    /// 一起进终态 ⇒ 一次唤醒、一个回合。任何「每回合 2 条」的限流都会让第三条永远
    /// 留在表里（此后不再有新终态，也就不会再有唤醒）。
    ///
    /// 变异：把 `take_completions(..)` 接回 `.take(2)` ⇒ 第三个标记不在该回合的
    /// `ModelRequest` 里、且 `has_pending` 仍为真 ⇒ 两条断言都变红。
    #[tokio::test]
    async fn all_pending_background_results_arrive_in_one_turn() {
        const MARKERS: [&str; 3] = ["BG-SWEEP-ALPHA", "BG-SWEEP-BETA", "BG-SWEEP-GAMMA"];

        let logs = TempDir::new("bg_sweep_logs");
        let _env = BgEnv::set(&logs.0);
        let calls = Arc::new(CallLog::default());
        let mut h =
            spawn_app_loop_backgrounds(Box::new(CountingModel(Arc::clone(&calls))), true).await;
        let (tasks, slot) = h
            .backgrounds
            .as_ref()
            .expect("本用例挂了后台上下文")
            .clone();
        let session = slot.get();
        let mut cleanup = BgCleanup::new(&tasks);
        for marker in MARKERS {
            let _ = cleanup.start_with(&session, &format!("sleep 0.2; echo {marker} >&2"));
        }

        // **不发任何请求** —— 唤醒必须自己起一轮，并在**同一个回合**里取完三条。
        let first = collect_until_view(&mut h).await;
        assert!(!first.is_empty(), "自动回合必须产生出站消息");

        let request = calls.requests().pop().expect("应有模型调用记录");
        let missing: Vec<&str> = MARKERS
            .iter()
            .copied()
            .filter(|m| !has_text(&request, m))
            .collect();
        assert!(
            missing.is_empty(),
            "同一个回合必须把三条结果全部带进上下文，缺 {missing:?}：{request:?}"
        );
        assert!(
            !tasks.has_pending(&session),
            "取完之后不得再有 `Pending`（否则没人会再唤醒它）"
        );
    }

    /// **`Wake::BackgroundReturned` 的复核**（端到端）：唤醒来自**别的会话**时不起回合。
    ///
    /// 唤醒是进程级的「可能有」：`sweep` 扫**所有**会话、`notify` 不携带会话信息 ⇒
    /// 别的会话的命令跑完也会叫醒本会话的循环。此时本会话没有待投结果 ⇒ 唤醒臂必须
    /// 原地 `continue`：既不发文案，也不花一次模型调用。
    //
    /// 构造走真实链路：给**另一个会话**起一条即刻退出的命令（`true`）⇒
    /// 共享 sweeper 下一拍看到 `newly_terminal > 0` ⇒ `notify` ⇒
    /// `Wake::BackgroundReturned` 臂 ⇒ 复核（本会话无 `Pending`）⇒ `continue`。
    /// 全程不发任何 `Request`：空闲态唯一可能的模型调用只能来自唤醒臂。
    ///
    /// 两条断言缺一不可：① 前提是别的会话**确实**有待投结果（否则本用例会因为
    /// 「根本没有唤醒」而假绿）；② 核心是模型调用数为 0。
    ///
    /// 变异：把唤醒臂的 `if !has_pending { continue }` 换成无条件起回合 ⇒ ② 变红
    /// （模型被调用一次）。只断言判据函数时该变异全绿 ⇒ 判据语义另由
    /// `background_turn_predicate_only_starts_for_own_pending` 单独钉住。
    #[tokio::test]
    async fn no_turn_starts_when_the_pending_set_is_empty() {
        const OTHER: &str = "other-session";

        let logs = TempDir::new("bg_empty_logs");
        let _env = BgEnv::set(&logs.0);
        let calls = Arc::new(CallLog::default());
        let h = spawn_app_loop_backgrounds(Box::new(CountingModel(Arc::clone(&calls))), true).await;
        let (tasks, slot) = h
            .backgrounds
            .as_ref()
            .expect("本用例挂了后台上下文")
            .clone();
        let session = slot.get();
        let cwd = h.temp_path("bg-cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut cleanup = BgCleanup::new(&tasks);
        // 本会话在起跑时是干净的 ⇒ 「零调用」断言的唯一输入只会来自下面那条别的会话的命令。
        assert!(
            !tasks.has_pending(&session),
            "前提：起跑时本会话没有待投结果"
        );

        // 属于**别的会话**的命令：直接经注册表起（不经 `cleanup.start_with` —— 那会用
        // 本会话 id），但要纳入收尾，免得用例失败时留下进程。
        cleanup.track_session(OTHER);
        let _handle = tasks
            .start(OTHER, "true", &cwd, None)
            .expect("起一条即刻退出的命令");

        // 等**两个** sweep tick（100ms 一拍）：第一拍让它进终态并播报唤醒，第二拍
        // 兜住「唤醒之后又顺手起了回合」的迟到调用。全程不发 `Request`。
        tokio::time::sleep(Duration::from_millis(400)).await;

        assert!(
            tasks.has_pending(OTHER),
            "前提：别的会话确实有待投结果（否则本用例会因为根本没有唤醒而假绿）"
        );
        assert_eq!(
            calls.count(),
            0,
            "唤醒来自别的会话 ⇒ 本会话不得起回合（模型调用数必须为 0）"
        );
    }

    /// **复核判据的语义**：只按**本会话**的 `Pending` 起回合。
    ///
    /// 唤醒臂用的是 [`should_start_background_turn`] ⇒ 这里钉住它的三种情形：空表、
    /// 别的会话有完成项、本会话有完成项（唯一起回合的那一种）。
    ///
    /// 变异：把复核改成「无条件起回合」⇒ 前两种情形都会返回 `true` ⇒ 变红。
    ///
    /// 用 `#[tokio::test]`：`start` 走 tokio 的子进程 ⇒ 与其它后台用例同规地留在
    /// runtime 内。
    #[tokio::test]
    async fn background_turn_predicate_only_starts_for_own_pending() {
        let root = temp_dir_for("bg_empty");
        let tasks = Arc::new(BackgroundTasks::new(
            root.clone(),
            ys_tools_basic::BgLimits::default(),
        ));

        // 空表：没有待投结果 ⇒ 不得起回合。
        assert!(
            !should_start_background_turn(&tasks, "s1"),
            "空表必须否决起回合"
        );

        // 别的会话有完成项、本会话没有 ⇒ 同样不得起回合（复核只按会话过滤）。
        let handle = tasks
            .start("other", "true", &root, None)
            .expect("起一条即刻退出的命令");
        tasks.mark_finished(handle, Some(0));
        assert!(tasks.has_pending("other"), "前提：别的会话确实有待投结果");
        assert!(
            !should_start_background_turn(&tasks, "s1"),
            "只按**本会话**复核 ⇒ 别的会话的结果不得起本会话的回合"
        );
        assert!(
            should_start_background_turn(&tasks, "other"),
            "本会话有待投结果 ⇒ 必须起回合"
        );
        tasks.kill_all("other");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **回合末的后台计数只进 debug 日志**：不进模型上下文、不进 transcript。
    ///
    /// 断言两件事：① debug 文件里有 `background_tasks` 记录且 `running` = 1；
    /// ② 出站消息里**没有**任何提到后台计数的 `Output`。
    ///
    /// 变异：把计数改成 `Outbound::Output` 投递 ⇒ 第二条断言变红；
    /// 删掉那条 debug 记录 ⇒ 第一条断言变红。
    #[tokio::test]
    async fn turn_end_records_the_running_background_count_in_debug_only() {
        let logs = TempDir::new("bg_debug_logs");
        let _env = DebugEnv::set(&logs.0);

        let model = MockModel::new("m");
        model.push_text("done");
        let mut h = spawn_app_loop_debug_backgrounds(Box::new(model), true).await;
        let (tasks, slot) = h
            .backgrounds
            .as_ref()
            .expect("本用例挂了后台上下文")
            .clone();
        let session = slot.get();
        let mut cleanup = BgCleanup::new(&tasks);
        let (_handle, _pid) = cleanup.start(&session);

        h.requests.send(prompt("hi")).await.unwrap();
        let msgs = collect_until_view(&mut h).await;
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, Outbound::Output(t) if t.contains("background"))),
            "后台计数不得进模型上下文：{msgs:?}"
        );
        shutdown(h).await;

        let files = debug_files(&logs.0);
        assert_eq!(files.len(), 1, "一个会话一个 debug 文件：{files:?}");
        let records = &files[0].1;
        let counts: Vec<&serde_json::Value> = records
            .iter()
            .filter(|record| record["ev"].as_str() == Some("background_tasks"))
            .collect();
        assert_eq!(
            counts.len(),
            1,
            "一个回合恰一条 `background_tasks` 记录：{records:?}"
        );
        assert_eq!(
            counts[0]["running"].as_u64(),
            Some(1),
            "回合末应记下「还有 1 个后台命令在跑」：{counts:?}"
        );
    }

    /// **回合内插话在模型给出最终答复时也不丢**（app 层端到端）。
    ///
    /// 模型直接给最终答复（无工具调用）那条路不经过下一轮 ⇒ 插话必须被**收场前**的
    /// 轮边界拉取接住，否则它随本回合的边界源一起销毁。
    ///
    /// 变异：把 `crates/ys-loop/src/basic.rs` 收场前的 `drain_boundary` 跳过 ⇒ 本用例红。
    #[tokio::test]
    async fn steer_during_final_answer_reaches_the_model() {
        const MARKER: &str = "STEER-DURING-FINAL";
        let (model, mut probe) = probe_model(ProbePlan::CompleteAtCalls(&[1]));
        let mut h = spawn_app_loop(Box::new(model), false, Vec::new()).await;

        h.requests.send(prompt("hi")).await.unwrap();
        // 等第 1 次 model 调用开始（此刻卡在 gate 上）。
        probe.wait_entered(1).await;
        // 模型正在生成最终答复时，用户插话。
        h.boundaries
            .send(Boundary::Steer(user_message(MARKER)))
            .await
            .unwrap();
        probe.let_app_task_pump().await;
        probe.release();

        let mut msgs = collect_until_view(&mut h).await;
        let _ = take_view(&mut msgs);

        let reqs = probe.requests();
        assert!(
            reqs.iter().any(|r| has_text(r, MARKER)),
            "插话必须被模型看到（不能随边界源销毁）：{reqs:?}"
        );
    }

    // ───────────── 提交后追问的 app_loop 层接线（一期「跨回合推进」）─────────────

    /// 追问开关的测试期状态：**整段持有 `env_lock`**，并清空
    /// `YUSHAN_FOLLOW_UP_ON_COMMIT`。
    ///
    /// `Config::follow_up_on_commit()` 里 env **优先于** `config.json`，故开发机上若把它
    /// 设成 `0`，用例在 `config.json` 里写 `true` 也白写（反之亦然）。持锁 + 清空
    /// 之后，开关的唯一来源就是本用例给的那份 `config.json`。
    ///
    /// 持锁 ⇒ 起 harness 必须传 `env_prelocked = true`（见 [`spawn_app_loop_prelocked`]）。
    /// 字段顺序即 drop 顺序：先恢复环境（此时锁仍活着），再释放锁。
    struct FollowUpEnv {
        _restore: crate::test_env::EnvRestore,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl FollowUpEnv {
        fn unset() -> Self {
            let guard = crate::test_env::env_lock();
            let restore =
                crate::test_env::EnvRestore::capture(&[crate::config::FOLLOW_UP_ON_COMMIT_ENV]);
            // SAFETY: 持有 env_lock，独占该变量的修改与恢复。
            unsafe { std::env::remove_var(crate::config::FOLLOW_UP_ON_COMMIT_ENV) };
            Self {
                _restore: restore,
                _guard: guard,
            }
        }
    }

    /// 起循环并**假定调用方已持有 `env_lock`**（`env_prelocked = true`）。
    ///
    /// 追问用例要整段独占 `YUSHAN_FOLLOW_UP_ON_COMMIT`，故只有这条起法可用
    /// （[`spawn_app_loop_with`] 会自己去抢同一把锁 ⇒ 同线程二次加锁死锁）。
    async fn spawn_app_loop_prelocked(
        model: Box<dyn Model>,
        tweak: impl FnOnce(&mut Config, &PathBuf),
    ) -> Harness {
        spawn_app_loop_full(
            model,
            false,
            Vec::new(),
            tweak,
            |b| b,
            |_| None,
            None,
            false,
            true,
            false,
        )
        .await
    }

    /// 在 `dir` 里跑一条 git 子命令（同步、阻塞）。
    ///
    /// 作者 / 提交者身份**每个变量都显式给**（不依赖全局 git 配置），并清掉
    /// `GIT_DIR` / `GIT_WORK_TREE`（父进程若导出它们，`-C` 的定位会被覆盖）。
    /// spawn 失败（未装 git）⇒ `None`。
    fn git_in(dir: &Path, args: &[&str]) -> Option<std::process::Output> {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "yushan-test")
            .env("GIT_AUTHOR_EMAIL", "yushan-test@example.invalid")
            .env("GIT_COMMITTER_NAME", "yushan-test")
            .env("GIT_COMMITTER_EMAIL", "yushan-test@example.invalid")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .ok()
    }

    /// 一次空提交（不碰工作区文件）。成功返回 `true`。
    fn commit_in(dir: &Path, message: &str) -> bool {
        git_in(dir, &["commit", "--allow-empty", "-q", "-m", message])
            .is_some_and(|out| out.status.success())
    }

    /// 在 `dir` 建一个**已有一次提交**的 git 仓库（判据要求前后 `HEAD` 都有值）。
    ///
    /// 未装 git / 初始化失败 ⇒ `None`：那种机器上判据恒为假，用例无事可验
    /// （与 `post_turn` 的用例同规，不把「环境缺 git」报成失败）。
    fn init_git_repo(dir: &Path) -> Option<()> {
        let init = git_in(dir, &["init", "-q"])?;
        if !init.status.success() {
            return None;
        }
        commit_in(dir, "init").then_some(())
    }

    // ---- 定时循环的端到端接线（`Wake::TimerFired`）----

    /// 定时循环用例的**环境护栏**：整段独占 `env_lock`、关 debug、日志落临时目录。
    ///
    /// 必须与 [`FollowUpEnv`] 同规：`spawn_app_loop_prelocked` 的契约是「调用方已持锁」，
    /// 而它据此**不再隔离** `YUSHAN_LOG_DIR`（`resolve_log_dir()` 未设时落
    /// `~/.yushan/logs`，且 debug 未设时**默认开启**）⇒ 不持锁的用例每跑一次
    /// 就往开发机写一批 `debug-*.jsonl`，同时把 `getenv` 暴露在并行 `set_var` 下。
    struct LoopEnv {
        _dir: TempDir,
        _restore: crate::test_env::EnvRestore,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl LoopEnv {
        fn new(tag: &str) -> Self {
            let guard = crate::test_env::env_lock();
            let dir = TempDir::new(tag);
            let restore = crate::test_env::EnvRestore::capture(&[
                crate::debug_log::DEBUG_ENV,
                crate::logging::LOG_DIR_ENV,
            ]);
            // SAFETY: 持有 env_lock，独占这两个变量的修改与恢复。
            unsafe {
                std::env::set_var(crate::debug_log::DEBUG_ENV, "0");
                std::env::set_var(crate::logging::LOG_DIR_ENV, dir.join("logs"));
            }
            Self {
                _dir: dir,
                _restore: restore,
                _guard: guard,
            }
        }
    }

    /// 定时循环用例的**统一前设**：开关关（本批不测提交追问）、cwd 指向真实仓库。
    ///
    /// `cwd` 必须经 `tweak` 在起循环前改到仓库 —— `/new` / `/resume` 会重算工作区，
    /// 但本批用例不碰它们。
    /// **锁只圈住 spawn**：`Config::from_env` / `resolve_log_dir` / `debug_enabled`
    /// 都在 spawn 期间读环境变量，此后 app 循环不再 `getenv`。把 `LoopEnv` 留到
    /// 用例结束会让 8 条用例整段串行（实测 2.6s → 9s），而收益为零。
    ///
    /// `LoopEnv` 在函数返回时 `Drop` ⇒ 恢复环境变量并释放锁；debug 文件已按
    /// 临时目录打开，后续写入不受影响。
    async fn loop_harness(model: Box<dyn Model>, repo: &Path) -> Harness {
        let env = LoopEnv::new("loop-env");
        let repo = repo.to_path_buf();
        let h = spawn_app_loop_prelocked(model, move |config, _tmp| {
            config.cwd = repo;
        })
        .await;
        drop(env);
        h
    }

    /// **到点即起回合**，且**不需要**任何外部状态变化。
    ///
    /// 工作区是**普通目录**（不是 git 仓库）：定时循环只认时间，不探外部信号。
    /// 用 1s 间隔（`parse_loop` 的下限是 10s，但协议层不钳制：钳制是 UI 的输入校验）。
    /// 断言调用数 == 1 同时钉住「到点就起」与「只起一个」。
    #[tokio::test]
    async fn loop_timer_fires_one_turn_in_non_git_dir() {
        let dir = TempDir::new("loop-fires");
        let plain = dir.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let log = Arc::new(CallLog {
            calls: AtomicU32::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let mut h = loop_harness(Box::new(CountingModel(Arc::clone(&log))), &plain).await;

        h.requests
            .send(Request::Loop {
                interval_secs: 1,
                prompt: "查 CI".into(),
                run_now: false,
            })
            .await
            .expect("投 /loop");
        collect_until_view(&mut h).await; // 登记本身刷一次视图

        // 定时器到点 ⇒ 起一个回合。等它的终局视图（模型每次调用都收工）。
        let msgs = collect_until_view(&mut h).await;
        // 从**模型侧请求**取证：`Role::Harness` 必须原样进请求（出站映射成 user
        // 角色是适配器的事，模型侧看到的仍是 user）。不能拿 `sessions::user_text`
        // 找正文 —— 它对非 `User` 角色恒返回 `None`（标题素材只取用户原话）。
        let harness_seen = log
            .requests()
            .iter()
            .any(|req| req.messages.iter().any(|m| m.role == Role::Harness));
        assert!(
            harness_seen,
            "定时回合必须以 Role::Harness 的输入进入历史（否则 TUI 画成用户自己敲的）"
        );
        assert_eq!(log.count(), 1, "到点 ⇒ 恰好一次模型调用");

        // 状态行必须出现 `[loop]` 段：`/loop` 生效后用户要能看到它在跑。
        let mut saw_loop_status = false;
        for m in &msgs {
            if let Outbound::View(v) = m
                && v.loop_state.is_some()
            {
                saw_loop_status = true;
            }
        }
        assert!(saw_loop_status, "起过定时回合后状态行必须有 loop 段");
        let _ = h.requests.send(Request::LoopOff).await;
    }

    /// **`/loop now` ⇒ 登记后首个回合立刻起**，不必等满一个间隔。
    ///
    /// 间隔取 3600s（远大于用例窗口）：若首个回合仍在等间隔，1s 窗口内调用数恒为 0
    /// —— 大间隔同时排除了「间隔太短所以看起来像立刻到点」这种解释。
    /// 工作区是普通目录（不是 git 仓库）：本形态同样只认时间。
    /// 结尾 `loop_state` 仍是 `Some`：`run_now` 只把**首次**唤醒提前，登记本身
    /// 不是一次性执行。
    ///
    /// 变异：把 `first` 恒取 `Instant::now() + interval`（忽略 `run_now`）⇒
    /// 下面的零调用断言变红。
    #[tokio::test]
    async fn loop_run_now_fires_immediately() {
        let dir = TempDir::new("loop-now");
        let plain = dir.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let log = Arc::new(CallLog {
            calls: AtomicU32::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let mut h = loop_harness(Box::new(CountingModel(Arc::clone(&log))), &plain).await;

        h.requests
            .send(Request::Loop {
                interval_secs: 3600,
                prompt: "查 CI".into(),
                run_now: true,
            })
            .await
            .expect("投 /loop now");
        collect_until_view(&mut h).await; // 登记本身刷一次视图

        // 1s 窗口 ≈ 间隔的 1/3600：窗口内落地就只能是「登记后立刻跑」。
        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert!(
            log.count() >= 1,
            "`/loop now` 必须在登记后立刻起首回合（间隔 3600s，1s 内零调用 ⇒ 仍在等间隔）"
        );

        // **先排空积压**：定时回合会发两帧视图（「正在跑」→「空闲」），
        // 直接再投请求会把它们误当成自己的应答。
        while let Ok(Some(_)) =
            tokio::time::timeout(Duration::from_millis(50), h.outbound.recv()).await
        {}
        h.requests
            .send(Request::SetThinking { visible: false })
            .await
            .expect("投一个无副作用请求以取一帧视图");
        let msgs = collect_until_view(&mut h).await;
        let state = msgs.iter().rev().find_map(|m| match m {
            Outbound::View(v) => Some(v.loop_state.clone()),
            _ => None,
        });
        assert!(
            matches!(state, Some(Some(_))),
            "`/loop now` 之后循环必须仍处于登记状态（提前首回合 ≠ 一次性执行）：{state:?}"
        );
        let _ = h.requests.send(Request::LoopOff).await;
    }

    /// **工作区一点变化都没有 ⇒ 到点照样起回合**。
    ///
    /// 本用例的前提是**不再有任何外部判据**：普通目录（非 git）、不提交、无外部信号，
    /// 调用数仍必须 ≥ 1。若这一条为零，说明循环又在等外部信号才动。
    #[tokio::test]
    async fn loop_tick_fires_without_workspace_changes() {
        let dir = TempDir::new("loop-idle");
        let plain = dir.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let log = Arc::new(CallLog {
            calls: AtomicU32::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let mut h = loop_harness(Box::new(CountingModel(Arc::clone(&log))), &plain).await;

        h.requests
            .send(Request::Loop {
                interval_secs: 1,
                prompt: "查 CI".into(),
                run_now: false,
            })
            .await
            .expect("投 /loop");
        collect_until_view(&mut h).await;

        // 2.5 个间隔 ⇒ 至少落地一个 tick。只断言下界：本条验「到点会动」，
        // 上界（不忙转）由 `loop_tick_does_not_busy_spin` 负责。
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            log.count() >= 1,
            "到点必须起回合（不存在任何外部判据，工作区没变也照样跑）"
        );
        let _ = h.requests.send(Request::LoopOff).await;
    }

    /// **`advance()` 的可观测后果：不推远就会忙转。**
    ///
    /// 1s 间隔、2.5s 窗口内：正确实现只落地 2 个 tick，留调度余量取上界 3；
    /// 缺 `advance()` 时 `sleep_until` 的截止时刻仍指向过去 ⇒ `select!` 立刻再次
    /// 就绪 ⇒ 调用数远超上界。
    ///
    /// 状态行是忙转的**独立证词**：`next_at` 停在过去 ⇒ 「还有多久」恒为 `0 秒后`。
    #[tokio::test]
    async fn loop_tick_does_not_busy_spin() {
        let dir = TempDir::new("loop-spin");
        let plain = dir.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let log = Arc::new(CallLog {
            calls: AtomicU32::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let mut h = loop_harness(Box::new(CountingModel(Arc::clone(&log))), &plain).await;

        h.requests
            .send(Request::Loop {
                interval_secs: 1,
                prompt: "查 CI".into(),
                run_now: false,
            })
            .await
            .expect("投 /loop");
        collect_until_view(&mut h).await;

        // 等 2.5 个间隔：正确实现落地 1～2 个回合（调度抖动允许第 3 个落地）。
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let calls = log.count();
        assert!(
            (1..=3).contains(&calls),
            "1s 间隔下 2.5s 内调用数应有界（1~3），实得 {calls}：\
             >3 说明缺 advance ⇒ 忙转；0 说明到点没起回合"
        );

        // **忙转的独立证词**：缺 `advance()` 时 `next_at` 永远停在过去 ⇒
        // 状态行的「还有多久」恒为 `0 秒后`。
        // **先排空积压**：定时回合会发两帧视图（「正在跑」→「空闲」），
        // `collect_until_view` 只取到第一帧。这里把已排队的全收走，再单独取新的一帧。
        while let Ok(Some(_)) =
            tokio::time::timeout(Duration::from_millis(50), h.outbound.recv()).await
        {}
        h.requests
            .send(Request::SetThinking { visible: false })
            .await
            .expect("投一个无副作用请求以取一帧视图");
        let msgs = collect_until_view(&mut h).await;
        let state = msgs.iter().rev().find_map(|m| match m {
            Outbound::View(v) => v.loop_state.clone(),
            _ => None,
        });
        let label = state.expect("循环在跑 ⇒ 视图必须有 loop 段").next_label;
        // 上面那次刷新取自 `advance()` 之后的 `next_at`；缺 `advance()` 时它是过去
        // 时刻 ⇒ `relative_label` 的向上取整给出 `0 秒后`（`saturating_duration_since`
        // 对过去时刻返回 0）。
        assert!(
            !label.as_deref().unwrap_or("0 秒后").starts_with("0 秒"),
            "下次唤醒不得是过去时刻（`0 秒后` ⇒ 缺 advance ⇒ 忙转）：{label:?}"
        );
        let _ = h.requests.send(Request::LoopOff).await;
    }

    /// **`/loop off` ⇒ 定时器立刻停**（已在跑的回合不中断，但后续不再唤醒）。
    ///
    /// 反向验证：若 `loop_state` 没被清，到点仍会起回合 —— 不依赖任何外部变化
    /// 就能触发，所以这个断言没有额外的前置条件。
    #[tokio::test]
    async fn loop_off_stops_further_ticks() {
        let dir = TempDir::new("loop-off");
        let plain = dir.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let log = Arc::new(CallLog {
            calls: AtomicU32::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let mut h = loop_harness(Box::new(CountingModel(Arc::clone(&log))), &plain).await;

        h.requests
            .send(Request::Loop {
                interval_secs: 1,
                prompt: "查 CI".into(),
                run_now: false,
            })
            .await
            .expect("投 /loop");
        collect_until_view(&mut h).await;

        h.requests
            .send(Request::LoopOff)
            .await
            .expect("投 /loop off");
        let msgs = collect_until_view(&mut h).await;
        // 关闭会回显一行（有循环在跑时才发）。
        assert!(
            msgs.iter()
                .any(|m| matches!(m, Outbound::Output(t) if t.contains("Loop stopped"))),
            "有循环在跑时 /loop off 必须回显确认"
        );

        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert_eq!(
            log.count(),
            0,
            "/loop off 之后不得再起回合（定时器必须真的停了）"
        );
    }

    /// **非 git 工作区照常登记**：循环只认时间，不预检工作区形态。
    ///
    /// 登记后必须真的会起回合 —— 否则状态行显示一个永不触发的循环，用户以为
    /// 在自动推进、实际一个回合都没起。
    #[tokio::test]
    async fn loop_registers_in_non_git_workspace() {
        let dir = TempDir::new("loop-nongit");
        let plain = dir.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let log = Arc::new(CallLog {
            calls: AtomicU32::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let mut h = loop_harness(Box::new(CountingModel(Arc::clone(&log))), &plain).await;

        h.requests
            .send(Request::Loop {
                interval_secs: 1,
                prompt: "查 CI".into(),
                run_now: false,
            })
            .await
            .expect("投 /loop");
        let msgs = collect_until_view(&mut h).await;

        assert!(
            !msgs.iter().any(|m| matches!(
                m,
                Outbound::Output(t) if t.contains("git")
            )),
            "非 git 工作区不得有任何关于 git 的提示：{msgs:?}"
        );
        let state = msgs.iter().rev().find_map(|m| match m {
            Outbound::View(v) => Some(v.loop_state.clone()),
            _ => None,
        });
        assert!(
            matches!(state, Some(Some(_))),
            "非 git 工作区也必须登记成功：{state:?}"
        );
        // 并且真的会起回合。
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            log.count() >= 1,
            "登记过的循环到点必须起回合（否则状态行谎报在跑）"
        );
        // 收掉循环，避免用例返回后它继续跑。
        let _ = h.requests.send(Request::LoopOff).await;
    }

    /// **`/loop off` 幂等**：没在跑时静默成功，不发任何文案。
    #[tokio::test]
    async fn loop_off_without_loop_is_silent() {
        let dir = TempDir::new("loop-off-idle");
        let log = Arc::new(CallLog {
            calls: AtomicU32::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let mut h = loop_harness(Box::new(CountingModel(Arc::clone(&log))), &dir.join("x")).await;
        h.requests
            .send(Request::LoopOff)
            .await
            .expect("投 /loop off");
        let msgs = collect_until_view(&mut h).await;
        // **不得产生任何 `Output`**，而不只是「不含 Loop stopped」——
        // 曾经走的是 `lines.push("")`：空 `Vec` 判非空 ⇒ 发一条空 `Output`
        // ⇒ transcript 里落一条空白 System 行。「不含某个字符串」这种断言
        // 对那个 bug 恒真。
        assert!(
            !msgs.iter().any(|m| matches!(m, Outbound::Output(_))),
            "没在跑时 /loop off 必须完全静默（连空行都不能发）：{msgs:?}"
        );
    }

    /// **`/new` 作废循环**：换会话后定时器不得再唤醒（否则旧定时器烧新会话的钱）。
    #[tokio::test]
    async fn new_session_clears_loop() {
        let dir = TempDir::new("loop-new");
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        if init_git_repo(&repo).is_none() {
            return;
        }
        let log = Arc::new(CallLog {
            calls: AtomicU32::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let mut h = loop_harness(Box::new(CountingModel(Arc::clone(&log))), &repo).await;

        h.requests
            .send(Request::Loop {
                interval_secs: 1,
                prompt: "查 CI".into(),
                run_now: false,
            })
            .await
            .expect("投 /loop");
        collect_until_view(&mut h).await;

        h.requests.send(Request::NewSession).await.expect("投 /new");
        collect_until_view(&mut h).await;

        assert!(commit_in(&repo, "after-new"), "外部提交失败");
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert_eq!(log.count(), 0, "/new 之后旧循环必须失效");
    }

    /// **`/clear` 作废循环**：与 `/new` 同规（循环归 session）。
    ///
    /// 变异：删掉 `Request::ClearSession` 臂的 `loop_state = None` ⇒ 本用例变红。
    #[tokio::test]
    async fn clear_session_clears_loop() {
        let dir = TempDir::new("loop-clear");
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        if init_git_repo(&repo).is_none() {
            return;
        }
        let log = Arc::new(CallLog {
            calls: AtomicU32::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let mut h = loop_harness(Box::new(CountingModel(Arc::clone(&log))), &repo).await;

        h.requests
            .send(Request::Loop {
                interval_secs: 1,
                prompt: "查 CI".into(),
                run_now: false,
            })
            .await
            .expect("投 /loop");
        collect_until_view(&mut h).await;

        h.requests
            .send(Request::ClearSession)
            .await
            .expect("投 /clear");
        collect_until_view(&mut h).await;

        assert!(commit_in(&repo, "after-clear"), "外部提交失败");
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert_eq!(log.count(), 0, "/clear 之后旧循环必须失效");
    }

    /// **`/resume` 到别的会话 ⇒ 作废循环并显式告知**（设计 §8 矩阵 #18）。
    ///
    /// 变异：删掉 `Request::ResumeSession` 臂里的 `loop_state.take()` ⇒ 本用例变红。
    #[tokio::test]
    async fn resume_to_other_session_stops_loop_with_notice() {
        let mut h = spawn_resumable_in_git(0, "resume-notice").await;
        let mut meta = ys_session::SessionMeta::new(0);
        meta.title = Some("目标会话".into());
        write_target_session(&h, "target-1", &meta, &[user_message("hello")]);

        // 先在**当前**会话设一条循环（`spawn_resumable` 不保证工作区是 git 仓库，
        // 故这里不依赖定时触发 —— 本用例只验「换会话时被作废并告知」）。
        let repo = h.temp_path("resume-repo");
        std::fs::create_dir_all(&repo).unwrap();
        let _ = init_git_repo(&repo);
        h.requests
            .send(Request::Loop {
                interval_secs: 3600,
                prompt: "查 CI".into(),
                run_now: false,
            })
            .await
            .expect("投 /loop");
        let reg = collect_until_view(&mut h).await;
        assert!(
            reg.iter()
                .any(|m| matches!(m, Outbound::View(v) if v.loop_state.is_some())),
            "前置：循环必须真的登记成功（工作区是 git 仓库）：{reg:?}"
        );

        h.requests
            .send(Request::ResumeSession {
                target: "target-1".into(),
            })
            .await
            .expect("投 /resume");
        let msgs = collect_until_view(&mut h).await;

        assert!(
            msgs.iter().any(|m| matches!(
                m,
                Outbound::Output(t) if t.contains("Loop stopped (session changed)")
            )),
            "换会话时必须显式告知循环已停（否则用户毫不知情地失去定时器）：{msgs:?}"
        );
        // 循环确实没了：后续任何请求的视图里都不该再有 loop 段。
        h.requests
            .send(Request::SetThinking { visible: false })
            .await
            .expect("取一帧视图");
        let msgs = collect_until_view(&mut h).await;
        let still = msgs.iter().rev().find_map(|m| match m {
            Outbound::View(v) => Some(v.loop_state.clone()),
            _ => None,
        });
        assert_eq!(still, Some(None), "换会话后视图不得再有 loop 段");
    }

    /// **`/resume` 到同一会话 ⇒ 循环保留**（没换会话就不该说 session changed）。
    ///
    /// 变异：把 `if switched && …` 退回 `if loop_state.take().is_some()` ⇒ 本用例变红。
    #[tokio::test]
    async fn resume_to_same_session_keeps_loop() {
        let mut h = spawn_resumable_in_git(0, "resume-same").await;
        // 让当前会话有落盘路径，才能「恢复自己」。
        h.requests
            .send(Request::Prompt(user_message("hi")))
            .await
            .expect("先跑一个回合把会话落盘");
        let _ = collect_until_view(&mut h).await;
        let path = h.session_path.get().expect("落盘后应有会话路径");
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("会话 id")
            .to_string();

        h.requests
            .send(Request::Loop {
                interval_secs: 3600,
                prompt: "查 CI".into(),
                run_now: false,
            })
            .await
            .expect("投 /loop");
        collect_until_view(&mut h).await;

        h.requests
            .send(Request::ResumeSession { target: id })
            .await
            .expect("投 /resume（同一会话）");
        let msgs = collect_until_view(&mut h).await;

        assert!(
            !msgs.iter().any(|m| matches!(
                m,
                Outbound::Output(t) if t.contains("Loop stopped")
            )),
            "恢复**同一**会话没换 session，不得误报循环已停：{msgs:?}"
        );
        let kept = msgs.iter().rev().find_map(|m| match m {
            Outbound::View(v) => Some(v.loop_state.clone()),
            _ => None,
        });
        assert!(
            matches!(kept, Some(Some(_))),
            "同一会话恢复后循环必须保留：{kept:?}"
        );
    }

    /// **`/loop` 覆盖不叠加**：再敲一次替换前一条（只有一个定时器）。
    ///
    /// 反向验证：若实现成了累加（两条并行），1s 间隔下调用数会翻倍。
    #[tokio::test]
    async fn loop_overwrites_instead_of_stacking() {
        let dir = TempDir::new("loop-overwrite");
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        if init_git_repo(&repo).is_none() {
            return;
        }
        let log = Arc::new(CallLog {
            calls: AtomicU32::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let mut h = loop_harness(Box::new(CountingModel(Arc::clone(&log))), &repo).await;

        // 两条都用长间隔（60s）：只要没被覆盖，两条都会在 60s 后各醒一次；
        // 而被覆盖后只剩最新一条。本用例只验「状态是单条」—— 用视图读出 interval。
        h.requests
            .send(Request::Loop {
                interval_secs: 60,
                prompt: "第一条".into(),
                run_now: false,
            })
            .await
            .expect("投第一条");
        collect_until_view(&mut h).await;
        h.requests
            .send(Request::Loop {
                interval_secs: 120,
                prompt: "第二条".into(),
                run_now: false,
            })
            .await
            .expect("投第二条");
        let msgs = collect_until_view(&mut h).await;

        let mut last: Option<ys_tui_coding::LoopView> = None;
        for m in msgs {
            if let Outbound::View(v) = m {
                last = v.loop_state.clone();
            }
        }
        let last = last.expect("视图里必须有 loop 段");
        assert_eq!(
            last.interval, "2m",
            "再敲 /loop 必须覆盖前一条（单条队列），实得 {:?}",
            last.interval
        );
        let _ = h.requests.send(Request::LoopOff).await;
    }

    // ---- 定时循环（`/loop`）的状态 ----

    /// `LoopState::advance` 把「下次唤醒」推到**未来**，且间隔被完整保留。
    ///
    /// 不推远 ⇒ `sleep_until` 立刻就绪 ⇒ 忙转。
    #[test]
    fn loop_state_advance_pushes_deadline_forward() {
        let interval = std::time::Duration::from_secs(300);
        let mut state = LoopState {
            next_at: Instant::now(),
            interval,
            prompt: "查 CI".into(),
        };
        let before = Instant::now();
        let next = state.advance();
        assert!(
            next > before,
            "advance 必须把截止时刻推到未来（否则 select! 立刻再次就绪）"
        );
        assert_eq!(state.next_at, next, "advance 的返回值就是新截止时刻");
        assert_eq!(state.interval, interval, "interval 不被 advance 改写");
    }

    /// 间隔的显示串：整小时 / 整分 / 秒三种形态。
    #[test]
    fn loop_state_interval_label() {
        let mk = |secs: u64| LoopState {
            next_at: Instant::now(),
            interval: std::time::Duration::from_secs(secs),
            prompt: String::new(),
        };
        assert_eq!(mk(7200).interval_label(), "2h");
        assert_eq!(mk(300).interval_label(), "5m");
        assert_eq!(mk(90).interval_label(), "90s");
        // 3661 不是整分也不是整时 ⇒ 落到秒（不丢精度，也不假装是 1h）。
        assert_eq!(mk(3661).interval_label(), "3661s");
    }

    /// 状态行的「还有多久」：秒 / 分 / 时+分三档。
    #[test]
    fn relative_label_formats_three_ranges() {
        use std::time::Duration;
        assert_eq!(relative_label(Duration::from_secs(0)), "0 秒后");
        assert_eq!(relative_label(Duration::from_secs(45)), "45 秒后");
        assert_eq!(relative_label(Duration::from_secs(300)), "5 分后");
        // 向上取整：0.9 秒是「1 秒后」，不是「0 秒后」（后者是个不存在的时刻）。
        assert_eq!(relative_label(Duration::from_millis(900)), "1 秒后");
        assert_eq!(relative_label(Duration::from_secs(7200)), "2 时 0 分后");
        // 不足一分钟按秒显示（59 秒不该被显示成「0 分后」）。
        assert_eq!(relative_label(Duration::from_secs(59)), "59 秒后");
    }

    /// 定时消息必须是 `Role::Harness`：它不是用户说的话。
    ///
    /// 角色错了会带来两个可见后果：TUI 把它画成用户行（历史像是用户自己每隔
    /// 5 分钟敲一句），且提示词正文的来源无从区分。
    #[test]
    fn loop_placeholder_is_harness_role() {
        let msg = loop_placeholder("检查 CI");
        assert_eq!(msg.role, Role::Harness, "定时消息不是用户输入");
        let text = match &msg.content[0] {
            ContentBlock::Text { text } => text.as_str(),
            other => panic!("期望文本块，实得 {other:?}"),
        };
        // 正文带前缀标出来源，并原样带上用户给的提示词。
        assert!(text.contains("[harness]"), "必须标出不是用户说的：{text}");
        assert!(text.contains("检查 CI"), "必须带上用户提示词：{text}");
    }

    /// **每次**模型调用都向仓库提交一次，然后回纯文本。
    ///
    /// 提交必须是**模型调用的副作用**：判据在 `run_turn_chain` 返回后的同一段同步代码里
    /// 取 `head_after`，测试侧等回合结束再提交就晚了。
    ///
    /// 「每次」而非「仅首次」：追问回合也会推进 `HEAD` ⇒ 任何「追问问完再判一次」
    /// 的实现都会冒出第 3 次调用，被调用次数断言抓住。
    struct CommitOnEveryCall {
        repo: PathBuf,
        log: Arc<CallLog>,
    }

    #[async_trait::async_trait]
    impl Model for CommitOnEveryCall {
        fn model_id(&self) -> &str {
            "git-committer"
        }

        async fn complete(
            &self,
            request: ModelRequest,
            _sink: &mut dyn ModelEventSink,
        ) -> Result<ModelResponse, ModelError> {
            let n = self.log.calls.fetch_add(1, Ordering::Relaxed);
            self.log.requests.lock().unwrap().push(request);
            assert!(
                commit_in(&self.repo, &format!("turn-{n}")),
                "回合计内的提交必须成功（git 不可用时用例应在开头就跳过）"
            );
            Ok(text_response("done"))
        }
    }

    /// 造一个 `HEAD` 可用的仓库 + 一个每次调用都提交的模型；git 不可用时返回 `None`
    /// （调用方直接跳过）。
    fn follow_up_fixture(tag: &str) -> Option<(TempDir, Arc<CallLog>)> {
        let repo = TempDir::new(tag);
        init_git_repo(&repo.0)?;
        Some((repo, Arc::new(CallLog::default())))
    }

    /// `config.json` 里打开追问开关，并把回合工作目录指向真实仓库。
    ///
    /// `cwd` 是判据的取点（`config.cwd`，不是 `worktree.get()`）—— 后者在共享工作区下
    /// 是 `None`，会让判据恒为假。
    fn enable_follow_up(config: &mut Config, tmp: &Path, repo: &Path) {
        let path = tmp.join("config.json");
        std::fs::write(&path, r#"{"follow_up_on_commit": true}"#).unwrap();
        config.load_user_config(&path);
        config.cwd = repo.to_path_buf();
        assert!(config.follow_up_on_commit(), "开关必须真的读到 true");
    }

    /// 某次请求里是否带了 `Role::Harness` 的文本消息。
    fn has_harness_message(request: &ModelRequest) -> bool {
        request
            .messages
            .iter()
            .any(|m| m.role == Role::Harness && has_text_in(m, "[harness]"))
    }

    /// 追问用例的出站条数上限。
    ///
    /// 正常情形一个请求只产出十十几条出站（两次回合的事件 + 一条 `View`），200 条
    /// 远超所需；而追问若退化成**无界循环**，事件流会无限增长 —— 那时单条 `recv_out`
    /// 的超时永远不会触发（消息一直有），表现为测试挂住。设上限后同一种缺陷表现为
    /// **失败**，与「死锁必须表现为失败」的要求一致。
    const MAX_OUTBOUND: usize = 200;

    /// [`collect_until_view`] 的有界版本：超过 [`MAX_OUTBOUND`] 条即失败。
    async fn collect_until_view_bounded(h: &mut Harness) -> Vec<Outbound<CodingView>> {
        let mut msgs = Vec::new();
        loop {
            let m = recv_out(&mut h.outbound).await;
            let is_view = matches!(m, Outbound::View(_));
            msgs.push(m);
            if is_view {
                return msgs;
            }
            assert!(
                msgs.len() < MAX_OUTBOUND,
                "一个请求已出站 {} 条仍未收场（疑似追问套娃/重试风暴）",
                msgs.len()
            );
        }
    }

    /// 提交后追问在 app_loop 层的**正面**接线：开关开 + 回合内产生提交 ⇒ 模型被调用
    /// **恰好 2 次**（主回合 1 次 + 追问回合 1 次）。
    ///
    /// 两件事同时钉住：
    ///
    /// - **P0（取点）**：走 `config.cwd`。若改回 `worktree.get()`，共享工作区下它是
    ///   `None` ⇒ `HEAD` 两侧都取不到 ⇒ 判据恒假 ⇒ 只有 1 次调用（断言红）。
    /// - **#7（防套娃）**：模型**每次都**提交，追问回合同样推进 `HEAD` ⇒ 任何「追问问完
    ///   再判一次」的实现都会冒出第 3 次调用。`if` 改成会重取 `HEAD` 的循环 ⇒ 本断言红
    ///   （调用次数不再停在 2）。
    ///
    /// 提交发生在**回合进行中**（模型调用的副作用），不是回合结束后由测试补 —— 后者
    /// `head_after` 早已取过，测不到判据。
    #[tokio::test]
    async fn app_loop_follow_up_appends_exactly_one_turn() {
        let _env = FollowUpEnv::unset();
        let Some((repo, log)) = follow_up_fixture("follow_up_on") else {
            // 未装 git：判据恒为假，本用例无事可验。
            return;
        };
        let repo_path = repo.0.clone();

        let model = CommitOnEveryCall {
            repo: repo_path.clone(),
            log: Arc::clone(&log),
        };
        let mut h = spawn_app_loop_prelocked(Box::new(model), move |config, tmp| {
            enable_follow_up(config, tmp, &repo_path);
        })
        .await;

        h.requests.send(prompt("hi")).await.unwrap();
        let mut msgs = collect_until_view_bounded(&mut h).await;
        let _ = take_view(&mut msgs);

        assert_eq!(
            log.count(),
            2,
            "主回合 1 次 + 追问回合 1 次，绝不能套娃：{msgs:?}"
        );

        // 追问确实发生了：第二个回合的输入是 harness 占位消息。
        let harness_events = events_of(&msgs)
            .iter()
            .filter(|env| {
                matches!(&env.event, AgentEvent::UserMessage { message } if message.role == Role::Harness)
            })
            .count();
        assert_eq!(harness_events, 1, "恰好一条追问输入：{msgs:?}");

        shutdown(h).await;
    }

    /// 开关**关**（默认）时同样有提交 ⇒ 模型只被调用 **1 次**。
    ///
    /// 同时覆盖 P1 的一半：开关关时根本不取 `HEAD`（不 spawn git），故判据无从为真。
    /// spawn 次数不可观测，这里只钉行为后果。
    #[tokio::test]
    async fn app_loop_follow_up_disabled_spawns_no_second_turn() {
        let _env = FollowUpEnv::unset();
        let Some((repo, log)) = follow_up_fixture("follow_up_off") else {
            return;
        };
        let repo_path = repo.0.clone();

        let model = CommitOnEveryCall {
            repo: repo_path.clone(),
            log: Arc::clone(&log),
        };
        // **不写 config.json**：开关保持默认关，`cwd` 仍指向真实仓库（证明「有提交」
        // 这件事本身不触发追加回合）。
        let mut h = spawn_app_loop_prelocked(Box::new(model), move |config, _tmp| {
            config.cwd = repo_path;
            assert!(!config.follow_up_on_commit(), "默认必须关");
        })
        .await;

        h.requests.send(prompt("hi")).await.unwrap();
        let mut msgs = collect_until_view_bounded(&mut h).await;
        let _ = take_view(&mut msgs);

        assert_eq!(log.count(), 1, "开关关时只跑主回合：{msgs:?}");
        assert!(
            !events_of(&msgs).iter().any(|env| matches!(
                &env.event,
                AgentEvent::UserMessage { message } if message.role == Role::Harness
            )),
            "开关关时不得出现追问输入：{msgs:?}"
        );

        shutdown(h).await;
    }

    /// 追问回合送进 `run_turn_chain` 的消息必须是 `Role::Harness` —— 两条独立证据：
    /// 事件流的 `UserMessage.role`，以及**模型实际收到的**请求里的消息角色。
    ///
    /// 前者决定 TUI 画成 System 行（不是用户行），后者决定模型看到的内容。
    /// 变异：把 `harness_input()` 换成 `Message { role: Role::User, .. }`（或干脆复用
    /// 上一条用户消息）⇒ 两条断言同时红。
    #[tokio::test]
    async fn app_loop_follow_up_uses_harness_role() {
        let _env = FollowUpEnv::unset();
        let Some((repo, log)) = follow_up_fixture("follow_up_role") else {
            return;
        };
        let repo_path = repo.0.clone();

        let model = CommitOnEveryCall {
            repo: repo_path.clone(),
            log: Arc::clone(&log),
        };
        let mut h = spawn_app_loop_prelocked(Box::new(model), move |config, tmp| {
            enable_follow_up(config, tmp, &repo_path);
        })
        .await;

        h.requests.send(prompt("hi")).await.unwrap();
        let mut msgs = collect_until_view_bounded(&mut h).await;
        let _ = take_view(&mut msgs);

        assert_eq!(
            log.count(),
            2,
            "追问必须真的发生（否则本用例测空）：{msgs:?}"
        );

        // 证据 ①：事件流。主回合是用户行（`User`），追问是 System 行（`Harness`）。
        let roles: Vec<Role> = events_of(&msgs)
            .iter()
            .filter_map(|env| match &env.event {
                AgentEvent::UserMessage { message } => Some(message.role.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            roles,
            vec![Role::User, Role::Harness],
            "追问回合的输入必须是 Harness 角色：{msgs:?}"
        );

        // 证据 ②：模型侧。首次请求里没有 harness 消息，追问那次请求里恰好有。
        let requests = log.requests();
        assert_eq!(requests.len(), 2, "应有两次模型调用：{requests:?}");
        assert!(
            !has_harness_message(&requests[0]),
            "主回合不该带 harness 消息：{requests:?}"
        );
        assert!(
            has_harness_message(&requests[1]),
            "追问回合必须把 harness 占位消息交给模型：{requests:?}"
        );

        shutdown(h).await;
    }
}
