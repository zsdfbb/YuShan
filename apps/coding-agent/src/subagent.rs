//! subagent 派发（刀 4）—— app 侧**真活**：造子执行环境、跑、落盘、截断、记账。
//!
//! 形态由 [ADR-0022](../../../docs/adr/0022-subagent-dispatch.md) 定死：
//! 子事件只在本地 `CollectingSink` 收集、**不进**父事件流；子表 = 父表去掉 `subagent`、
//! 结果 = 硬截断摘要 + 产物路径 + `StopReason`、取消 = `ChildBoundary` 只透传
//! `is_aborted`。落地裁定见 [ADR-0029](../../../docs/adr/0029-subagent-landing-decisions.md)：
//! 每回合派发总量上限、`model` 名解析失败 fail-closed、产物目录 sweep + 环境变量覆盖口。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use ys_component::RunLimits;
use ys_core::{ContentBlock, Message, Role, StopReason, ToolCallId, Usage};
use ys_event::{AgentEvent, CollectingSink};
use ys_loop::AgentInput;
use ys_model::Model;
use ys_protocol::{Boundary, BoundarySource, QueueBoundarySource};
use ys_runtime::{AgentBuilder, AgentPorts};
use ys_session::{MemorySession, Session};
use ys_tool::{
    SubagentOutcome, SubagentRunner, Tool, ToolContext, ToolError, ToolResult, ToolSpec,
};
use ys_tools_web::WebConfig;

/// 回传摘要的**硬上限**（截断责任在代码，不在提示词）。
pub const MAX_SUMMARY_BYTES: usize = 8 * 1024;
/// 每回合派发次数上限（ADR-0022 决策 15；ADR-0029 §5①）。超限返回**响亮的 `Err`**。
pub const DEFAULT_MAX_DISPATCHES_PER_TURN: u32 = 16;
/// 产物保留期 —— 启动期 sweep 超过此龄的文件（只对 Primary 档）。
pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// 派发工具外闸相对**子预算**的宽限：让协作路径（watchdog → 轮边界）先赢，
/// 外闸只兜「连轮边界都到不了」的挂死（ADR-0029 修正 2）。
pub const OUTER_GRACE: Duration = Duration::from_secs(60);
/// 单条诊断的硬上限（含 `[diagnostic:` 前缀与截断标记）。
const MAX_DIAGNOSTIC_BYTES: usize = 1024;
const DIAGNOSTIC_PREFIX: &str = "[diagnostic: ";

/// 跨回合最多滞留多少条**未投出**的后台结果。
///
/// 与 [`DEFAULT_MAX_DISPATCHES_PER_TURN`] 同量级：一个回合最多派发 16 次，
/// 若全都在回合末之后完成，下一回合要能全部投出去。超出时**淘汰最旧**并
/// 留一行日志（结果本身仍能从产物转录里找回 —— 那才是权威副本）。
pub const MAX_PENDING_RESULTS: usize = 32;

/// 子 agent 被取消时的可归因来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelCause {
    ParentTurnCancelled,
    ChildBudget,
    Unknown,
}

impl CancelCause {
    fn as_str(self) -> &'static str {
        match self {
            Self::ParentTurnCancelled => "parent-turn-cancelled",
            Self::ChildBudget => "child-budget",
            Self::Unknown => "unknown",
        }
    }
}

/// 子 agent 的边界源：`is_aborted` 透传父的；`take()` **只取自己那份**，绝不消费父队列。
/// 与 ADR-0022 决策 5 的差异（`take()` 从「恒 None」改为取自己的）记在 ADR-0029 §6 修正 4。
/// **插话探针不透传**（恒 `false`）—— 理由见该 trait 方法实现。
pub struct ChildBoundary {
    own: Arc<QueueBoundarySource>,
    parent: Option<Arc<dyn BoundarySource>>,
}

impl ChildBoundary {
    /// `parent` 必须是 `Arc` —— 边界源要能 coerce 成 `Arc<dyn BoundarySource>`
    /// （默认隐含 `'static`），而借用父的引用做不到（自身会带上父的生命周期）。
    pub fn new(parent: Option<Arc<dyn BoundarySource>>) -> Self {
        Self {
            own: Arc::new(QueueBoundarySource::new()),
            parent,
        }
    }

    /// 自己那份边界源（watchdog 往这里 `push(Abort)`）。
    pub fn own(&self) -> Arc<QueueBoundarySource> {
        self.own.clone()
    }
}

/// `Debug` 只输出「是否有父」—— **不打印父队列内容**（避免泄露 steer 正文）。
impl std::fmt::Debug for ChildBoundary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildBoundary")
            .field("has_parent", &self.parent.is_some())
            .finish()
    }
}

impl BoundarySource for ChildBoundary {
    fn take(&self) -> Option<Boundary> {
        self.own.take()
    }
    fn is_aborted(&self) -> bool {
        self.own.is_aborted() || self.parent.as_ref().is_some_and(|p| p.is_aborted())
    }

    /// **恒 `false`**：子的工具不得因**父的**插话被打断。
    ///
    /// 插话是给父模型的输入，爹的 `Steer` 在父的轮边界才被消费、且孩子的上下文
    /// 里根本看不到它 —— 若让它透传，父的插话会以「孩子命令莫名提前中止」的形式
    /// 表现出来，而这个中止在孩子那边会催生一条「用户插话了」的结果，语义互相矛盾。
    /// 孩子只该因**中止**（自己那份或父的 `Abort`）而收口。
    fn has_pending_steer(&self) -> bool {
        false
    }
}

/// 子 agent usage 的累加器（跨并发派发无锁）。父回合末 drain 进 `TurnStats`。
#[derive(Default)]
pub struct UsageTotals {
    input: AtomicU64,
    output: AtomicU64,
    hit: AtomicU64,
    /// 未命中累计。与 `hit` 配对：**只在 `hit` 上报时才累加对应的 miss**，
    /// 不可从聚合后的 `input − hit` 派生（那会把没上报的子调用整个 input 算成未命中）。
    miss: AtomicU64,
    /// 是否有任一子调用上报了缓存命中读数（`Some(0)` 也算上报）。
    hit_reported: AtomicBool,
    /// 推理 token 累计；仅当至少一次子调用上报了 `Some(_)` 时才有意义
    /// （是否上报由 `reasoning_reported` 记录）。
    reasoning: AtomicU64,
    /// 是否有任一子调用上报了 `reasoning_tokens`（`Some(0)` 也算上报）。
    reasoning_reported: AtomicBool,
}

impl UsageTotals {
    /// 记账一个**子 agent 回合结果**。
    ///
    /// `cache` 取 `RunResult::cache_reading`（loop 的**逐轮**累加值），
    /// **不得**从 `run.usage` 派生：那是回合聚合，`input_tokens` 对没上报的轮也累加过，
    /// 而 `hit` 缺席时不参与相加 —— 减法会把缺报轮整段算成未命中。
    pub fn add_run(&self, run: &ys_loop::RunResult) {
        self.add(&run.usage, run.cache_reading);
    }

    /// 记账一份用量 + 一份**显式的**缓存读数（`None` = 没有读数）。
    ///
    /// `hit` 与 `miss` 必须在**同一条件**下累加，否则聚合后无法靠减法还原。
    pub fn add(&self, u: &Usage, cache: Option<(u32, u32)>) {
        self.input
            .fetch_add(u.input_tokens as u64, Ordering::Relaxed);
        self.output
            .fetch_add(u.output_tokens as u64, Ordering::Relaxed);
        // 只有上报了才累加并置位：`None` = 未上报，`Some(0)` = 真 0，两者不可折叠。
        if let Some((h, m)) = cache {
            self.hit.fetch_add(h as u64, Ordering::Relaxed);
            self.miss.fetch_add(m as u64, Ordering::Relaxed);
            self.hit_reported.store(true, Ordering::Relaxed);
        }
        if let Some(v) = u.reasoning_tokens {
            self.reasoning.fetch_add(v as u64, Ordering::Relaxed);
            self.reasoning_reported.store(true, Ordering::Relaxed);
        }
    }

    /// 取走累计值并清零（回合末一次）。
    ///
    /// `prompt_cache_hit_tokens`：任何子调用报过 ⇒ `Some(累计)`（含累计为 0 的情形）；
    /// 一次都没报过 ⇒ `None`。
    /// `prompt_cache_miss_tokens` 恒 `None` —— 它等于 `input − hit`，不落地。
    /// `reasoning_tokens` 同 `hit`：任何子调用报过 ⇒ `Some(累计)`；一次都没报过 ⇒ `None`。
    /// 两者都不能恒写 0 —— 那会把「未上报」与「真 0」折叠，且会把已上报的值静默丢弃。
    pub fn drain(&self) -> (Usage, Option<(u32, u32)>) {
        let hit_reported = self.hit_reported.swap(false, Ordering::Relaxed);
        let hit = self.hit.swap(0, Ordering::Relaxed) as u32;
        let miss = self.miss.swap(0, Ordering::Relaxed) as u32;
        let reported = self.reasoning_reported.swap(false, Ordering::Relaxed);
        let reasoning = self.reasoning.swap(0, Ordering::Relaxed) as u32;
        let usage = Usage {
            input_tokens: self.input.swap(0, Ordering::Relaxed) as u32,
            output_tokens: self.output.swap(0, Ordering::Relaxed) as u32,
            prompt_cache_hit_tokens: hit_reported.then_some(hit),
            prompt_cache_miss_tokens: None,
            reasoning_tokens: reported.then_some(reasoning),
        };
        // `miss` 必须**单独**带出：`usage.cache_split()` 会拿聚合后的 `input`
        // 去减，而 `input` 对没上报的子调用也累加过 —— 那会把它们的整个 input
        // 算成未命中（与 `RunResult::cache_reading` 同一个坑）。
        (usage, hit_reported.then_some((hit, miss)))
    }
}

/// 一条**已跑完、尚未送达**的后台派发结果。
///
/// 只在「完成时本回合已结束」或「完成时落点未绑定」两条路径上入表 ——
/// 正常情况下结果直接进本回合的边界源，不经过这里。
struct PendingResult {
    handle: String,
    message: Message,
}

/// 后台派发结果的**进程级**投递表。
///
/// # 为何需要（生产证据，会话 `1792840206_799154000`）
///
/// `tokio::spawn` 是**分离任务**（`JoinHandle` 已丢弃，无 abort 路径）：子 agent
/// 在父回合结束后仍会继续跑（日志实测：turn 11 于 20:14:28 结束，子转录
/// 20:16:59 / 20:20:47 才落盘）。若完成时只 `push` 进**回合局部**的边界源，
/// 那条结果会随回合一起销毁 —— 子 agent 白跑，模型永远等不到。
///
/// 故结果先落到本表（跨回合存活），再由每回合开头补投进**当前**边界源。
/// 与后台命令（`bash_bg` 的 `BackgroundTasks::deferred`）**同一套形态**：
/// 注册表是进程级的、落点是每回合绑定的。
#[derive(Default)]
pub struct PendingResults {
    inner: Mutex<PendingInner>,
    /// 有结果落地时的一次性唤醒 —— **让回合外的结果也能被看见**。
    ///
    /// 未绑定时结果只是静默入表（下面 `deliver` 的分支），而表在回合外**没有读者**
    /// （`take_pending` 唯一的入口是 `Request::Prompt`）⇒ 不加唤醒，用户会以为
    /// 「子 agent 还在跑」而它其实早就回来了。
    ///
    /// # 为什么是 `notify_one`（存 permit），而不是 `notify_waiters`
    ///
    /// 消费端（app 循环的空闲分支）是**事件驱动**的：大部分时间并不挂在 `notified()` 上。
    /// `notify_waiters` **不存 permit** ⇒ 若结果正好在那段间隙里落地，唤醒就落空了，
    /// 「已返回」那行提示要等到下一次（可能永远不来的）结果或用户输入。
    /// `notify_one` 存一个 permit，下一次 `notified()` 立即完成。
    ///
    /// 代价（已接受）：permit 可跨回合残留 ⇒ 空闲等待可能「假醒」一次；
    /// 消费端用 [`has_pending`](PendingResults::has_pending) 复核，空则无动作。
    wake: tokio::sync::Notify,
}

#[derive(Default)]
struct PendingInner {
    /// 本回合的投递落点（`None` = 未绑定 ⇒ 结果留表等下回合补投）。
    sink: Option<Arc<QueueBoundarySource>>,
    /// 绑定时那格的代数 —— 解绑只在代数仍相同时生效，
    /// 避免晚 drop 的旧守卫把新回合刚绑的落点抹掉。
    generation: u64,
    /// 未送达的结果，**入表顺序**（最旧在前）。
    queue: Vec<PendingResult>,
}

impl PendingResults {
    /// 绑定本回合的落点 —— 守卫 drop 时解绑。
    ///
    /// 后一次绑定替换前一次（同一回合重复绑定会换到新队列，最后一个生效）。
    pub fn bind(self: &Arc<Self>, sink: Arc<QueueBoundarySource>) -> PendingResultsBinding {
        let generation = {
            let mut inner = self.lock();
            inner.generation = inner.generation.wrapping_add(1);
            inner.sink = Some(sink);
            inner.generation
        };
        PendingResultsBinding {
            results: Arc::clone(self),
            generation,
        }
    }

    fn unbind(&self, generation: u64) {
        let mut inner = self.lock();
        if inner.generation == generation {
            inner.sink = None;
        }
    }

    /// 交付一条结果：**已绑定** ⇒ 直接进本回合边界源；**未绑定** ⇒ 留表。
    ///
    /// 返回 `true` = 已投出（结果不再留表）。
    fn deliver(self: &Arc<Self>, handle: String, message: Message) -> bool {
        // **先取落点、释放锁，再 push**：`push` 会唤醒等待者，持锁调外部代码是坏习惯。
        let sink = {
            let mut inner = self.lock();
            if let Some(sink) = inner.sink.clone() {
                sink
            } else {
                // 未绑定（回合已结束 / 尚未开始）⇒ 留表，下一回合补投。
                if inner.queue.len() >= MAX_PENDING_RESULTS {
                    let dropped = inner.queue.remove(0);
                    crate::logging::log(&format!(
                        "subagent: 未投出的结果超过 {MAX_PENDING_RESULTS} 条，淘汰最旧的一条（{}）—— \
                         完整转录仍在该次派发的产物文件里",
                        dropped.handle
                    ));
                }
                inner.queue.push(PendingResult { handle, message });
                drop(inner);
                // 唤醒在**锁外**：持锁调外部代码是坏习惯（`push` 也会唤醒等待者）。
                self.wake.notify_one();
                return false;
            }
        };
        sink.push(Boundary::Deferred { handle, message });
        true
    }

    /// 是否还有未投出的结果 —— [`notified`](Self::notified) 唤醒后的**必要复核**。
    ///
    /// 唤醒只是「可能有新东西」（`notify_waiters` 不区分事件类型，也可能来自别的
    /// 写入方）⇒ 醒来后必须复核，不能假定一定非空。
    pub fn has_pending(&self) -> bool {
        !self.lock().queue.is_empty()
    }

    /// 等到「有新结果入表」。
    ///
    /// 只报「**可能有**」，不报具体是什么 —— 消费端一律 `take_pending()` 全量取走，
    /// 并先用 [`has_pending`](Self::has_pending) 复核（permit 可能跨回合残留）。
    pub async fn notified(&self) {
        self.wake.notified().await;
    }

    /// 取走全部未投出的结果（**当前回合**的补投入口）—— 按入表顺序。
    ///
    /// 本表是进程级的、会话无关：子 agent 的结果不像后台命令那样带会话归属
    /// （子 agent 在哪个会话里跑完，结果就属于哪里；跨会话串台的风险由
    /// 「一条结果只入表一次」保证）。
    pub fn take_pending(&self) -> Vec<(String, Message)> {
        let mut inner = self.lock();
        inner
            .queue
            .drain(..)
            .map(|p| (p.handle, p.message))
            .collect()
    }

    /// 未投出条数（**仅供测试** —— 生产路径不需要它，故不进非测试构建）。
    #[cfg(test)]
    pub fn pending_len(&self) -> usize {
        self.lock().queue.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PendingInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// [`PendingResults::bind`] 的 RAII 守卫 —— drop 时解绑本回合的落点。
pub struct PendingResultsBinding {
    results: Arc<PendingResults>,
    generation: u64,
}

impl Drop for PendingResultsBinding {
    fn drop(&mut self) {
        self.results.unbind(self.generation);
    }
}

/// 进程级的「仍在跑的子 agent 数」（ADR-0055）。
///
/// # 为何必须是进程级
///
/// `AppSubagentRunner` **每回合新建**（`app_loop` 的 `make_runner`）⇒ 上一回合派发、
/// 本回合仍在跑的子 agent 不在它的视野里。若判据读 runner 自己的计数器，它会恒为 `0`
/// ⇒ 父回合立刻收场，「等子 agent 回来」静默失效。
///
/// 故本结构与 [`PendingResults`] 并列：以 `Arc` 经 `SubagentSetup` 跨 `/resume` 重建共享。
///
/// # 为何用 RAII 守卫而不是手工加减
///
/// `ChildTask::execute` 有四条退出路径（组装失败 / `Ok` / `Err` / future 被 drop）。
/// 手工 `fetch_sub` 必漏一条，而漏减会让父回合钉死在等待里 —— 这是正确性问题，
/// 不是防御性编程。守卫进 `ChildTask`，`Drop` 结构上保证回归。
#[derive(Default)]
pub struct LiveSubagents {
    running: AtomicUsize,
    /// 计数变化时唤醒等待方。
    ///
    /// 用 `notify_waiters`（**不存 permit**）而非 `notify_one`：等待循环是
    /// 「唤醒 + `subagent_poll` tick」双源 ⇒ 漏一次唤醒的代价只是一个 tick；
    /// 而 permit 会跨回合残留，让下一回合凭空立即醒来。
    settled: tokio::sync::Notify,
}

impl LiveSubagents {
    /// 登记一个在跑的子 agent；返回的守卫 drop 时自减并唤醒等待方。
    ///
    /// **必须在 `tokio::spawn` 之前调用**（但要在模型 / 额度解析成功之后）——
    /// 否则「已 spawn 但未计数」的窗口里父回合可能收场。
    pub fn enter(self: &Arc<Self>) -> LiveGuard {
        self.running.fetch_add(1, Ordering::SeqCst);
        LiveGuard {
            live: Arc::clone(self),
        }
    }

    /// 仍在跑的数量。父回合的**唯一收场判据**（`BasicLoop` Step 9c）。
    pub fn count(&self) -> usize {
        self.running.load(Ordering::SeqCst)
    }

    /// 等到「计数发生变化」—— **不分方向**，调用方必须复核 [`count`](Self::count)。
    pub async fn settled_notified(&self) {
        self.settled.notified().await
    }
}

/// [`LiveSubagents::enter`] 的 RAII 守卫 —— drop 时自减并唤醒。
pub struct LiveGuard {
    live: Arc<LiveSubagents>,
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        // `fetch_sub` 的结果不作判断：`SeqCst` 下计数不会穿透 0（enter/drop 严格配对）。
        self.live.running.fetch_sub(1, Ordering::SeqCst);
        // 必须唤醒：判据从「>0」变成「==0」的那一刻，父回合才允许收场。
        self.live.settled.notify_waiters();
    }
}

/// **ADR-0055 承重不变量**：`LiveGuard` 必须把计数减回去。
///
/// 父回合的收场判据是 `running() == 0` ⇒ 任何让计数停在 >0 的路径都会把父回合
/// 钉死在等待里直到 `subagent_wait` 上限。`ChildTask::execute` 有四条退出路径
/// （组装失败 / `Ok` / `Err` / future 被 drop），靠 `Drop` 才能全部覆盖。
///
/// 变异：删掉 `LiveGuard::drop` 里的 `fetch_sub` ⇒ 归零断言变红。
#[test]
fn live_guard_returns_the_slot_on_drop() {
    let live = Arc::new(LiveSubagents::default());
    assert_eq!(live.count(), 0, "初始无在跑");

    {
        let _a = live.enter();
        let _b = live.enter();
        assert_eq!(live.count(), 2, "两次 enter ⇒ 计数 2");
    }
    assert_eq!(live.count(), 0, "守卫 drop 后必须归零");
}

/// `has_pending` 与 `take_pending` 口径一致 —— app 侧空闲唤唤醒靠前者判断
/// 「要不要提示」，后者负责真正取走。
///
/// 变异：把 `has_pending` 写成恒 `false` ⇒ 入表后的断言变红。
#[test]
fn pending_results_has_pending_matches_the_queue() {
    let results = Arc::new(PendingResults::default());
    assert!(!results.has_pending(), "初始为空");

    let delivered = results.deliver(
        "sa1".to_string(),
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "r".into() }],
        },
    );
    assert!(!delivered, "未绑定落点时不算已投出");
    assert!(results.has_pending(), "入表后 has_pending 必须为真");

    let taken = results.take_pending();
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].0, "sa1");
    assert!(!results.has_pending(), "取走后必须为空");
}

/// **唤醒必须存 permit**：结果在「消费端还没开始等」时落地，也不能丢通知。
///
/// 这是 `notify_waiters` → `notify_one` 那次修复的钉子。消费端（app 循环空闲分支）
/// 是事件驱动的，大部分时间不挂在 `notified()` 上；`notify_waiters` 在那个间隙里
/// 唤醒落空 ⇒「已返回」提示要等下一条结果（可能永远不来）。
///
/// 变异：把 `deliver` 里的 `notify_one` 换回 `notify_waiters` ⇒ 本用例超时失败。
#[tokio::test]
async fn pending_results_wakeup_survives_no_waiter() {
    let results = Arc::new(PendingResults::default());

    // **先**入表（此刻没有任何 waiter），**再**开始等 —— 这是真实时序：
    // 子 agent 收尾时 app 循环多半正阻塞在 `request_rx.recv()` 上。
    results.deliver(
        "sa1".to_string(),
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "late".into(),
            }],
        },
    );
    assert!(results.has_pending(), "应已入表");

    // 若唤醒没存 permit，这里会永远挂住。
    tokio::time::timeout(Duration::from_secs(2), results.notified())
        .await
        .expect("唤醒必须在无 waiter 时存下 permit，否则迟到的结果永远不会被看见");
}

/// 产物目录。`YUSHAN_SUBAGENTS_DIR` 显式指定时是 **Override 档（只写不删）**；
/// 否则是 `~/.yushan/subagents/`（Primary 档，启动期可 sweep）。
pub struct SubagentsDir {
    path: PathBuf,
    sweep: bool,
}

impl SubagentsDir {
    /// 解析落点。`Override` 档**只写不删**（用户点名的目录不得被清理）。
    pub fn resolve() -> Self {
        if let Ok(dir) = std::env::var("YUSHAN_SUBAGENTS_DIR")
            && !dir.is_empty()
        {
            return Self {
                path: PathBuf::from(dir),
                sweep: false,
            };
        }
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        Self {
            path: home.join(".yushan").join("subagents"),
            sweep: true,
        }
    }

    /// 测试注入用：显式落点，避免测试经 [`resolve`](Self::resolve) 把转录写进真实的
    /// `~/.yushan/subagents/`。`sweep` 仅在为 `true` 时启用启动期清理。
    #[cfg(test)]
    pub fn at(path: PathBuf, sweep: bool) -> Self {
        Self { path, sweep }
    }

    /// 启动期清理：只对 Primary 档生效，删超过 [`RETENTION`] 的文件。
    pub fn sweep_old(&self) {
        if !self.sweep {
            return;
        }
        let Ok(entries) = std::fs::read_dir(&self.path) else {
            return;
        };
        let now = std::time::SystemTime::now();
        for e in entries.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let Ok(age) = now.duration_since(modified) else {
                continue;
            };
            if age > RETENTION {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    /// 写一份子转录，返回绝对路径。文件名含进程 id + 进程内原子序号（并行安全）。
    fn write_transcript(&self, body: &str) -> std::io::Result<PathBuf> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::fs::create_dir_all(&self.path)?;
        let path = self.path.join(format!("{}_{}.md", std::process::id(), seq));
        std::fs::write(&path, body)?;
        Ok(std::fs::canonicalize(&path).unwrap_or(path))
    }
}

/// 按名构造的模型实例缓存，键为「模型名 + 构建时的有效推理强度」。
///
/// 把有效强度纳入键，是为了让 `/effort` 或配置默认变化后，同名子 Agent 不以
/// 旧档位复用既有实例。已知边界：键不含 provider / api_base —— 二者变化不会
/// 使缓存失效。
pub(crate) type SubagentModelCache = Arc<Mutex<HashMap<(String, Option<String>), Arc<dyn Model>>>>;

/// app 侧的派发实现：每回合构造一份（计数器天然 per-turn）。
pub struct AppSubagentRunner<'a> {
    parent_model: Option<Arc<dyn Model>>,
    /// 按名构造模型用（`model` 参数）；`None` = 未接线 ⇒ 指定模型名一律 fail-closed。
    config: Option<&'a crate::config::Config>,
    /// 按「模型名 + 构建时档位」缓存实例（跨派发复用连接池 + TLS）；缓存持
    /// `Arc<dyn Model>`，因为 `SubagentRunner::run` 需要 `&dyn Model`，`Box`
    /// 拿不出稳定引用。
    model_cache: SubagentModelCache,
    child_prompt: String,
    child_limits: RunLimits,
    child_budget: Duration,
    workspace: PathBuf,
    web: WebConfig,
    dir: Arc<SubagentsDir>,
    max_dispatches: u32,
    dispatched: AtomicU32,
    totals: Arc<UsageTotals>,
    /// 父回合的边界源（共享所有权）—— 子 agent 的 `ChildBoundary` 要透传它的
    /// 中止状态。
    ///
    /// **必须是 `Arc` 而非借用**：`ChildBoundary` 要能进 `Arc<dyn BoundarySource>`
    /// （隐含 `'static`），借用父会把它拽出 `'static`。
    parent_boundary: Option<Arc<dyn BoundarySource>>,
    /// 后台结果的**跨回合**投递表。
    ///
    /// 子 agent 完成时本回合可能已经结束（`tokio::spawn` 是分离任务）⇒ 不能只
    /// 往回合本地的边界源里 `push`（那份队列随回合销毁）。故统一交给本表：
    /// 已绑定落点就直接投，否则留表等下回合补投。详见 [`PendingResults`]。
    results: Arc<PendingResults>,
    /// 进程级的「仍在跑」计数（ADR-0055）—— 父回合的收场判据经它读出。
    live: Arc<LiveSubagents>,
}

impl<'a> AppSubagentRunner<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        parent_model: Option<Arc<dyn Model>>,
        config: Option<&'a crate::config::Config>,
        model_cache: SubagentModelCache,
        child_prompt: String,
        child_limits: RunLimits,
        child_budget: Duration,
        workspace: PathBuf,
        web: WebConfig,
        dir: Arc<SubagentsDir>,
        totals: Arc<UsageTotals>,
        parent_boundary: Option<Arc<dyn BoundarySource>>,
        results: Arc<PendingResults>,
        live: Arc<LiveSubagents>,
    ) -> Self {
        // 子 agent 输出**不进用户屏幕**（只交回最终 summary）⇒ 流中断后丢弃已收到的
        // 部分输出、重发同一请求是安全的（`Err` 分支不 `append` 消息，会话未被污染）。
        //
        // 用 1 次重试：流中断是偶发故障（实测 6307 次 attempt 中 4 次，均在单个
        // 网关故障窗口内）；一次就能拦住绝大多数，再多只会放大真正的持续故障。
        //
        // 在此统一设置（而非调用方）⇒ `new` / `shared` 两个入口不可能漂移。
        let mut child_limits = child_limits;
        child_limits.stream_interrupt_retries = 1;

        Self {
            parent_model,
            config,
            model_cache,
            child_prompt,
            child_limits,
            child_budget,
            workspace,
            web,
            dir,
            max_dispatches: DEFAULT_MAX_DISPATCHES_PER_TURN,
            dispatched: AtomicU32::new(0),
            totals,
            parent_boundary,
            results,
            live,
        }
    }
}

/// 装配期冻结的派发输入：每回合据此造一份 [`AppSubagentRunner`]。
///
/// 冻结的是**子 agent 的固定前缀**（`child_prompt`）与**子预算**（`child_limits` /
/// `child_budget`）—— 缓存相干性要求它们不含任何时间 / 会话相关量。
pub struct SubagentSetup {
    child_prompt: String,
    child_limits: RunLimits,
    child_budget: Duration,
    workspace: PathBuf,
    web: WebConfig,
    dir: Arc<SubagentsDir>,
    /// 按「模型名 + 构建时档位」缓存实例（跨派发复用；缓存持 `Arc<dyn Model>`，
    /// 见 runner 的注释）。
    model_cache: SubagentModelCache,
    /// 后台结果的**跨回合**投递表（`Arc` 跨重建共享）。
    ///
    /// 与 `dir` 同理以 `Arc` 传入：工作区切换（`/resume`）会重建本结构，但
    /// **已在后台跑着的子 agent 并不随之重建** —— 它们完成时写的那份表必须与
    /// 重建后的新表是**同一个**，否则结果又落进无人读的旧表。
    results: Arc<PendingResults>,
    /// 进程级的「仍在跑」计数（ADR-0055）。
    ///
    /// 与 `results` 同一条理由共享所有权：已在后台跑着的子 agent 不随 `/resume` 重建，
    /// 它们的计数必须汇入**同一份**计数器，否则新回合读到的 `0` 会让父回合提前收场。
    live: Arc<LiveSubagents>,
}

impl SubagentSetup {
    /// 产物目录**由装配方显式给出**（不在构造期隐式读 `YUSHAN_SUBAGENTS_DIR` / `HOME`）——
    /// 落点决策与 `WebConfig::from_env` / `sweep_fetched` 同在装配点完成，且测试可注入
    /// 临时目录，不会写进用户的 `~/.yushan/subagents/`。
    ///
    /// `dir` 以 `Arc` 传入、**跨重建共享**（供 [`crate::assembly::AgentFactory`] 用）：
    /// 工作区切换时同一进程内会多次重建本结构，共享同一份产物目录描述（`Arc` 只复制
    /// 指针），落点与 sweep 档位因此跨重建一致。
    /// 后台结果的**跨回合**投递表（`Arc` 跨重建共享）。
    ///
    /// 与 `dir` 同理以 `Arc` 传入：工作区切换（`/resume`）会重建本结构，但
    /// **已在后台跑着的子 agent 并不随之重建** —— 它们完成时写的那份表必须与
    /// 重建后的新表是**同一个**，否则结果又落进无人读的旧表。
    ///
    /// `live` 同理（见 [`LiveSubagents`] 的文档）。
    ///
    /// 参数多是必然的：它冻结的是「一次派发需要的全部不变输入」，每个都得能在
    /// `/resume` 重建后重新给到。拆参数包只会把六个 `Arc::clone` 推给调用方。
    ///
    /// 注意：`child_limits` 里的 `stream_interrupt_retries` 由调用方给，本入口**不**覆写
    /// —— 它与 [`Self::new`] 不同：后者是产品入口，必须无条件开启流中断重试；
    /// 本入口给测试与恢复路径用，保留调用方的选择。
    #[allow(clippy::too_many_arguments)]
    pub fn shared(
        child_prompt: String,
        child_limits: RunLimits,
        child_budget: Duration,
        workspace: PathBuf,
        web: WebConfig,
        dir: Arc<SubagentsDir>,
        results: Arc<PendingResults>,
        live: Arc<LiveSubagents>,
    ) -> Self {
        Self {
            child_prompt,
            child_limits,
            child_budget,
            workspace,
            web,
            dir,
            model_cache: Arc::new(Mutex::new(HashMap::new())),
            results,
            live,
        }
    }

    /// 派发工具的外闸（子预算 + 宽限，构造期注入）。
    pub fn outer_timeout(&self) -> Duration {
        self.child_budget + OUTER_GRACE
    }

    /// 绑定**本回合**的结果落点 —— 守卫 drop 时解绑。
    ///
    /// 调用方必须让守卫活到回合结束（绑定给具名变量，不写 `_`）：中途解绑会让
    /// 本回合后半程完成的结果只能留表，平白晚一个回合送达。
    pub fn bind_results(&self, sink: Arc<QueueBoundarySource>) -> PendingResultsBinding {
        self.results.bind(sink)
    }

    /// 取走**上一回合遗留**的完成结果（本回合开头的补投入口）。
    ///
    /// 子 agent 在回合结束后完成 ⇒ 结果留表；这里把它们交给调用方泵进本回合的
    /// 边界源。**不在这里 push** —— 落点由调用方持有（与 `bash_bg` 的补投同形）。
    pub fn take_pending_results(&self) -> Vec<(String, Message)> {
        self.results.take_pending()
    }

    /// 造一份 runner（每回合一次 ⇒ 派发计数器天然 per-turn）。
    pub fn make_runner<'a>(
        &self,
        config: Option<&'a crate::config::Config>,
        parent_model: Option<Arc<dyn Model>>,
        totals: Arc<UsageTotals>,
        parent_boundary: Option<Arc<dyn BoundarySource>>,
    ) -> AppSubagentRunner<'a> {
        AppSubagentRunner::new(
            parent_model,
            config,
            self.model_cache.clone(),
            self.child_prompt.clone(),
            self.child_limits.clone(),
            self.child_budget,
            self.workspace.clone(),
            self.web.clone(),
            self.dir.clone(),
            totals,
            parent_boundary,
            self.results.clone(),
            self.live.clone(),
        )
    }

    /// 结果投递表（供 app 循环在**回合之间**等它落地）。
    pub fn pending_results(&self) -> &Arc<PendingResults> {
        &self.results
    }
}

#[async_trait]
impl SubagentRunner for AppSubagentRunner<'_> {
    async fn run(
        &self,
        prompt: &str,
        model: Option<&str>,
        boundary: Option<&dyn BoundarySource>,
    ) -> Result<SubagentOutcome, String> {
        // `boundary` 参数保留在 trait 签名里（`ToolContext` 的通用挂载点），但
        // 本实现用 `self.parent_boundary`（生产路径上两者是同一个 `Arc`）——
        // 它要跟着 `ChildTask` 进后台，故从构造期注入的那份取。
        let _ = boundary;

        // fail-closed：未支持按名构造模型时**绝不静默回退到父模型**。
        let resolved = self.resolve_model(model)?;
        self.take_dispatch_slot()?;
        let child_limits = self.child_limits_for(model);

        // 执行交给 owned 的 `ChildTask` —— 同步 `run` 与异步 `spawn` **共用同一份**
        // 执行逻辑（否则两条路径的行为会漂移：结果打包、诊断、usage 上卷全在手写两份）。
        //
        // 同步路径也占额：模型自己就在等这一发，但父回合的收场判据必须看到它
        // （否则「在等的子 agent」不计入 `running()` ⇒ 判据偏乐观）。
        Ok(self
            .child_task(resolved, child_limits, self.live.enter())
            .execute(prompt.to_string())
            .await)
    }

    /// 进程级：仍在跑的子 agent 数。
    ///
    /// **读 `self.live` 而非自己的计数器** —— 后者只记「本回合派了几次」
    /// （`dispatched`，每回合从 0 重来），拿它当判据会漏掉上一回合遗留的子 agent。
    fn running(&self) -> usize {
        self.live.count()
    }

    fn settled(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(self.live.settled_notified())
    }

    /// **后台派发**：回合内解析模型 + 查额度（两者都要读 `config`，不能带进后台），
    /// 之后把 owned 的 `ChildTask` 交给 `tokio::spawn`。
    ///
    /// 任务完成时把结果作为 `Boundary::Deferred` **推进父边界源** ⇒ `BasicLoop` 在
    /// 下一个轮边界 append 进会话并 emit `DeferredResult`。**模型不需要 poll。**
    ///
    /// # 生命周期：结果**跨回合送达**
    ///
    /// 父回合**不等**异步子 agent（它的价值是「父回合能继续」）。但「不等」不等于
    /// 「不送达」：`tokio::spawn` 是分离任务，子 agent 在回合结束后仍会继续跑
    /// （实测：转录在回合结束后 2~6 分钟才落盘）。它的结果交给 [`PendingResults`]——
    /// 完成时本回合仍活着就直接进边界源，否则留表、由**下一回合开头**补投。
    ///
    /// 故模型即使在本回合没等到，下一回合开头也一定会看到它；不会再出现
    /// 「子 agent 跑完了、结果却随回合销毁」这种白烧一次派发的情形。
    ///
    /// # 孤儿约束
    ///
    /// 任务本身仍受 `child_budget` watchdog 约束（最迟到预算时以 `Cancelled` 收尾、转录照写）。
    fn spawn(
        &self,
        prompt: &str,
        model: Option<&str>,
        _boundary: Option<&dyn BoundarySource>,
    ) -> Result<String, String> {
        let resolved = self.resolve_model(model)?;
        self.take_dispatch_slot()?;
        let child_limits = self.child_limits_for(model);
        let results = self.results.clone();

        // 句柄 = 本回合第 N 次派发（`take_dispatch_slot` 已自增 ⇒ 直接取当前值）。
        // 只用于让模型 / UI 区分是哪个后台任务，**不需要全局唯一**。
        let handle = format!("sa{}", self.dispatched.load(Ordering::Relaxed));
        // **先占额、再 spawn**：`ChildTask` 持有守卫 ⇒ `execute` 的每条退出路径
        // （组装失败 / Ok / Err / future 被 drop）都由 `Drop` 回归。
        let task = self.child_task(resolved, child_limits, self.live.enter());
        let prompt = prompt.to_string();
        let handle_for_task = handle.clone();

        tokio::spawn(async move {
            let outcome = task.execute(prompt).await;
            // 渲染成一条消息交给投递表：本回合还活着 ⇒ 直接进边界源；
            // 已结束 ⇒ 留表，下一回合开头补投。角色用 `User` 沿压缩摘要的先例
            // （ADR-0017 决策 12：系统注入走 `User`）—— **区分来源靠事件类型**
            // （`DeferredResult`），不是靠角色。
            results.deliver(
                handle_for_task,
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: render_outcome_text(&outcome),
                    }],
                },
            );
        });
        Ok(handle)
    }
}

/// 把一次派发的结果渲染成**纯文本**（供 `Boundary::Deferred` 的消息正文）。
///
/// 与工具结果那份（`SubagentTool::call` 里的拼装）内容是同一套：
/// 诊断前缀 → 摘要 → 转录指针。抽成函数是为了两条路径**不漂移**。
fn render_outcome_text(out: &SubagentOutcome) -> String {
    let mut content = String::new();
    if let Some(d) = &out.diagnostic {
        content.push_str(d);
    }
    if !out.summary.is_empty() {
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&out.summary);
    }
    if let Some(a) = &out.artifact {
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&format!("[full transcript: {a}]"));
    }
    content
}

impl AppSubagentRunner<'_> {
    /// 把「一次派发」需要的**全部输入**收成 [`ChildTask`]。
    ///
    /// `model` / `child_limits` 由调用方**在回合内**解析好（解析要读 `config`，
    /// 而 `config` 是借用、不可 `'static`）⇒ 收成的 `ChildTask` 只含 owned 字段。
    /// 解析要用的模型（`model` 参数 → 缓存 / 构造；`None` → 父模型）。
    ///
    /// 抽出来是因为 `run` 与 `spawn` **都要**它，而 fail-closed 纪律不能写两遍。
    fn resolve_model(&self, model: Option<&str>) -> Result<Arc<dyn Model>, String> {
        let owned: Option<Arc<dyn Model>> = match model {
            None => None,
            Some(name) => {
                // 键含构建时档位：`/effort` 只重建主模型，若这里只按名缓存，
                // 同名派发会命中旧实例（旧档位）。
                let key = (
                    name.to_string(),
                    self.config.and_then(|c| c.effective_reasoning_effort()),
                );
                if let Some(hit) = self.model_cache.lock().unwrap().get(&key).cloned() {
                    Some(hit)
                } else {
                    let built = self
                        .config
                        .and_then(|c| c.build_model_for(name))
                        .ok_or_else(|| {
                            format!(
                                "按名指定模型 `{name}` 无法构造（未配置凭证 / 名字非法）—— \
                                 fail-closed，不静默回退到父模型"
                            )
                        })?;
                    let arc: Arc<dyn Model> = Arc::from(built);
                    self.model_cache.lock().unwrap().insert(key, arc.clone());
                    Some(arc)
                }
            }
        };
        match owned {
            Some(m) => Ok(m),
            None => self
                .parent_model
                .clone()
                .ok_or_else(|| "无可用模型（未登录 / 未配置）".to_string()),
        }
    }

    /// 占用一个**本回合**派发额度（同步与后台派发**共用**同一个计数器）。
    fn take_dispatch_slot(&self) -> Result<(), String> {
        let n = self.dispatched.fetch_add(1, Ordering::Relaxed);
        if n >= self.max_dispatches {
            return Err(format!(
                "本回合派发次数已达上限（{}），拒绝继续派发（防成本爆炸）",
                self.max_dispatches
            ));
        }
        Ok(())
    }

    /// 子换 model ⇒ 窗口必须按**子模型**取（继承父窗口是**静默错**：32k 模型按 1M
    /// 判 ⇒ 不压缩 ⇒ 撞窗）。默认继承时不改（与父同值）。
    ///
    /// 必须走 `effective_context_window_for`：父级封顶（`context_cap_tokens`）对子
    /// agent 同样生效，否则子 agent 会绕过上限。
    fn child_limits_for(&self, model: Option<&str>) -> RunLimits {
        let mut limits = self.child_limits.clone();
        if let (Some(cfg), Some(name)) = (self.config, model) {
            limits.context_window = cfg.effective_context_window_for(name);
        }
        limits
    }

    fn child_task(
        &self,
        model: Arc<dyn Model>,
        child_limits: RunLimits,
        guard: LiveGuard,
    ) -> ChildTask {
        ChildTask {
            model,
            child_prompt: self.child_prompt.clone(),
            child_limits,
            child_budget: self.child_budget,
            workspace: self.workspace.clone(),
            web: self.web.clone(),
            dir: self.dir.clone(),
            totals: self.totals.clone(),
            parent_boundary: self.parent_boundary.clone(),
            _live: guard,
        }
    }
}

/// 一次子 agent 执行的**全部 owned 输入** ⇒ 满足 `'static`，可交给 `tokio::spawn`。
///
/// 存在的理由：`AppSubagentRunner` 持有 `config` 的**借用**，不能跨任务存活；
/// 而异步派发的后台任务必须比回合活得久（至少活到父回合结束）。把执行所需的
/// 所有东西搬进本结构，借用就留在回合内。
struct ChildTask {
    model: Arc<dyn Model>,
    child_prompt: String,
    child_limits: RunLimits,
    child_budget: Duration,
    workspace: PathBuf,
    web: WebConfig,
    dir: Arc<SubagentsDir>,
    totals: Arc<UsageTotals>,
    parent_boundary: Option<Arc<dyn BoundarySource>>,
    /// 本子 agent 的「在跑」额度（ADR-0055）。
    ///
    /// 字段名带下划线：**不读取，只为了持有** —— `execute` 返回与 future 被 drop
    /// 两条路径都靠它的 `Drop` 把计数减回去。少了它，父回合会钉死在等待里。
    _live: LiveGuard,
}

impl ChildTask {
    /// 跑完一个子 agent，返回结果。
    ///
    /// **永远返回 `Ok`**：子 agent「跑完了但没跑好」走 `stop_reason` + `diagnostic`
    /// （ADR-0001 错误结果通道），只有「建不起来」（工具表装配失败）才是 `Err` 语义 ——
    /// 那种情形在这里折成 `Cancelled` + 诊断文本，与既有 `run` 的 `Err` 分支同形。
    async fn execute(self, prompt: String) -> SubagentOutcome {
        let started = Instant::now();
        let parent_cancelled_probe = self.parent_boundary.clone();
        let (tools, _hint) = crate::toolset::build_tools(&self.workspace, self.web.clone());
        let mut child = match AgentBuilder::new()
            .tools(tools)
            .limits(self.child_limits.clone())
            .working_dir(self.workspace.clone(), self.workspace.clone())
            .system_prompt(self.child_prompt.clone())
            .build()
        {
            Ok(child) => child,
            Err(e) => {
                // 建不起来：留一份最小转录 + 诊断。同样是**故障**，不是取消。
                let stop_reason = StopReason::Failed;
                let cause = None;
                let details = vec![format!("子 agent 组装失败: {e}")];
                let diagnostic = run_diagnostic(&stop_reason, cause, 0, &details);
                let artifact = self
                    .dir
                    .write_transcript(&format!(
                        "# 子 agent 未启动\n\n{}\n",
                        diagnostic.as_deref().unwrap_or("(无诊断)")
                    ))
                    .ok();
                return SubagentOutcome {
                    summary: String::new(),
                    artifact: artifact.map(|p| p.display().to_string()),
                    stop_reason,
                    diagnostic,
                    usage: Usage::default(),
                };
            }
        };

        let child_boundary = ChildBoundary::new(self.parent_boundary.clone());
        // 自己那份边界源（watchdog 往这里 `push(Abort)`）。先取出 `Arc` 副本 ——
        // `child_boundary` 本身要 move 进 `Arc`（供 `AgentPorts` 持有）。
        let child_own = child_boundary.own();
        // watchdog：到点向**子自己的**边界源 push(Abort)，子在轮边界优雅收尾。
        // 必须 `AbortOnDropHandle` —— 否则外闸 drop 子 future 时该任务会孤儿化并沉睡到预算到点。
        let handle = {
            let own = child_own.clone();
            let budget = self.child_budget;
            tokio::spawn(async move {
                tokio::time::sleep(budget).await;
                own.push(Boundary::Abort);
            })
        };
        let _watchdog_guard = tokio_util::task::AbortOnDropHandle::new(handle);

        let mut session = MemorySession::new();
        let mut sink = CollectingSink::new();
        let result = child
            .run_turn(
                AgentInput::text(format!("{prompt}{CHILD_TASK_SUFFIX}")),
                AgentPorts::new(
                    Some(&*self.model),
                    &mut session,
                    &mut sink,
                    Some(Arc::new(child_boundary)),
                ),
            )
            .await;
        let elapsed_ms = trusted_elapsed_ms(started);
        let raw_tool_results = collect_raw_tool_results(sink.events());

        match result {
            Ok(run) => {
                self.totals.add_run(&run);
                let summary =
                    ensure_files_section(&transcript_summary(&session, run.final_message.as_ref()));
                let stop_reason = run.stop_reason;
                let cause = cancel_cause(
                    &stop_reason,
                    is_parent_cancelled(parent_cancelled_probe.as_deref()),
                    child_own.is_aborted(),
                );
                let mut details = Vec::new();
                let body = transcript_body(
                    &session,
                    &raw_tool_results,
                    &stop_reason,
                    cause,
                    elapsed_ms,
                    run_diagnostic(&stop_reason, cause, elapsed_ms, &details).as_deref(),
                );
                let artifact = match self.dir.write_transcript(&body) {
                    Ok(path) => Some(path),
                    Err(error) => {
                        details.push(format!("transcript write failed: {error}"));
                        None
                    }
                };
                let diagnostic = run_diagnostic(&stop_reason, cause, elapsed_ms, &details);
                SubagentOutcome {
                    summary,
                    artifact: artifact.map(|p| p.display().to_string()),
                    stop_reason,
                    diagnostic,
                    usage: run.usage,
                }
            }
            Err(e) => {
                // 「跑都跑不起来」仍尽量留转录；错误文本只进诊断。
                //
                // 用 `Failed` 而非 `Cancelled`：这里是**故障**（provider 错误、
                // 流中断等），不是用户取消。归成 `Cancelled` 会让诊断行变成
                // `stop_reason=cancelled cause=unknown error=...provider error...`
                // —— 自相矛盾，且把真故障埋在「用户取消」里。
                let stop_reason = StopReason::Failed;
                // 不是取消 ⇒ 不该有取消归因（`cancel_cause` 已按此语义实现）。
                let cause = None;
                let mut details = vec![format!("error={e}")];
                let body = transcript_body(
                    &session,
                    &raw_tool_results,
                    &stop_reason,
                    cause,
                    elapsed_ms,
                    run_diagnostic(&stop_reason, cause, elapsed_ms, &details).as_deref(),
                );
                let artifact = match self.dir.write_transcript(&body) {
                    Ok(path) => Some(path),
                    Err(error) => {
                        details.push(format!("transcript write failed: {error}"));
                        None
                    }
                };
                let diagnostic = run_diagnostic(&stop_reason, cause, elapsed_ms, &details);
                SubagentOutcome {
                    summary: String::new(),
                    artifact: artifact.map(|p| p.display().to_string()),
                    stop_reason,
                    diagnostic,
                    usage: Usage::default(),
                }
            }
        }
    }
}
fn trusted_elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}

fn is_parent_cancelled(boundary: Option<&dyn BoundarySource>) -> bool {
    boundary.is_some_and(|source| source.is_aborted())
}

/// 只有 `Cancelled` 有 `cause`；父取消优先于子预算，无法归因时为 `Unknown`。
fn cancel_cause(
    stop_reason: &StopReason,
    parent_cancelled: bool,
    child_cancelled: bool,
) -> Option<CancelCause> {
    if !matches!(stop_reason, StopReason::Cancelled) {
        return None;
    }
    Some(if parent_cancelled {
        CancelCause::ParentTurnCancelled
    } else if child_cancelled {
        CancelCause::ChildBudget
    } else {
        CancelCause::Unknown
    })
}

fn stop_reason_label(stop_reason: &StopReason) -> &'static str {
    match stop_reason {
        StopReason::Completed => "completed",
        StopReason::MaxRounds => "max-rounds",
        StopReason::Cancelled => "cancelled",
        StopReason::Truncated => "truncated",
        StopReason::NoOutput => "no-output",
        StopReason::Failed => "failed",
        _ => "unknown",
    }
}

fn run_diagnostic(
    stop_reason: &StopReason,
    cause: Option<CancelCause>,
    elapsed_ms: u64,
    details: &[String],
) -> Option<String> {
    if matches!(stop_reason, StopReason::Completed) && details.is_empty() {
        return None;
    }

    let mut payload = format!(
        "stop_reason={} elapsed_ms={elapsed_ms}",
        stop_reason_label(stop_reason)
    );
    if let Some(cause) = cause {
        payload.push_str(&format!(" cause={}", cause.as_str()));
    }
    for detail in details {
        payload.push(' ');
        payload.push_str(detail);
    }
    Some(render_diagnostic(&payload))
}

/// 清洗控制字符并保证最终整条诊断不超过 [`MAX_DIAGNOSTIC_BYTES`]。
fn render_diagnostic(payload: &str) -> String {
    let sanitized: String = payload
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let full = format!("{DIAGNOSTIC_PREFIX}{sanitized}]");
    if full.len() <= MAX_DIAGNOSTIC_BYTES {
        return full;
    }

    const ELLIPSIS: &str = "...";
    let budget = MAX_DIAGNOSTIC_BYTES - DIAGNOSTIC_PREFIX.len() - ELLIPSIS.len();
    let mut end = budget;
    while end > 0 && !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    format!("{DIAGNOSTIC_PREFIX}{}{ELLIPSIS}", &sanitized[..end])
}

fn collect_raw_tool_results(events: &[AgentEvent]) -> HashMap<ToolCallId, String> {
    let mut results = HashMap::new();
    for event in events {
        if let AgentEvent::ToolResult { id, result } = event {
            results.insert(id.clone(), result.content.clone());
        }
    }
    results
}

/// 追加在子 agent 任务正文之后的一行要求：让子 agent 按 `## Files` 节汇报产物。
///
/// **放 messages 不放 system** —— 放 system 就是每轮前缀变动（破缓存）。
const CHILD_TASK_SUFFIX: &str = "\n\nWhen you are done, end your reply with a `## Files` section \
     listing the files you created or changed (write `(none)` if you changed nothing).";

/// 保证摘要尾部有一段**固定形状**的产物节（ADR-0022 决策 16）。
///
/// 把「这一段在不在」从**子 agent 的自觉**变成 **runner 的保证**：父能可靠区分
/// 「没产物」与「忘了报」。**只保证格式，不保证内容完整**。
fn ensure_files_section(summary: &str) -> String {
    if summary.contains("## Files") {
        return summary.to_string();
    }
    format!("{summary}\n\n## Files\n(none)")
}

/// 从子会话取一段给父看的摘要（**硬截断**）。
fn transcript_summary(session: &MemorySession, final_message: Option<&ys_core::Message>) -> String {
    let text = final_message
        .map(text_of)
        .filter(|t| !t.trim().is_empty())
        .or_else(|| {
            session.messages().iter().rev().find_map(|m| {
                let t = text_of(m);
                (!t.trim().is_empty()).then_some(t)
            })
        })
        .unwrap_or_default();
    truncate_bytes(&text, MAX_SUMMARY_BYTES)
}

fn text_of(m: &ys_core::Message) -> String {
    m.content
        .iter()
        .filter_map(|b| match b {
            ys_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn transcript_body(
    session: &MemorySession,
    raw_tool_results: &HashMap<ToolCallId, String>,
    stop_reason: &StopReason,
    cause: Option<CancelCause>,
    elapsed_ms: u64,
    diagnostic: Option<&str>,
) -> String {
    let mut out = String::new();
    for m in session.messages() {
        out.push_str(&format!("## {:?}\n", m.role));
        for block in &m.content {
            match block {
                ContentBlock::Text { text } => {
                    out.push_str(text);
                    out.push('\n');
                }
                ContentBlock::ToolUse {
                    id,
                    name,
                    arguments,
                } => {
                    out.push_str(&format!("[tool_use id={} name={name}]\n", id.0));
                    out.push_str(
                        &serde_json::to_string_pretty(arguments)
                            .unwrap_or_else(|_| arguments.to_string()),
                    );
                    out.push('\n');
                }
                ContentBlock::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                } => {
                    let content = raw_tool_results
                        .get(tool_call_id)
                        .map(String::as_str)
                        .unwrap_or(content);
                    out.push_str(&format!(
                        "[tool_result id={} is_error={is_error}]\n{content}\n",
                        tool_call_id.0
                    ));
                }
                _ => out.push_str("[unknown content block]\n"),
            }
        }
        out.push('\n');
    }
    out.push_str("## Run\n");
    out.push_str(&format!("stop_reason={}\n", stop_reason_label(stop_reason)));
    if let Some(cause) = cause {
        out.push_str(&format!("cause={}\n", cause.as_str()));
    }
    out.push_str(&format!("elapsed_ms={elapsed_ms}\n"));
    out.push_str("diagnostic=");
    out.push_str(diagnostic.unwrap_or("(none)"));
    out.push('\n');
    out
}

/// 按**字符边界**截断到 ≤ `max` 字节（UTF-8 安全）。
fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…[truncated]", &s[..end])
}

/// 同步派发等待期间探「用户插话」的轮询间隔。
///
/// 与 `ys-tools-basic` 的 `ABORT_POLL_INTERVAL` **同口径**（100ms）：那里不能跨 crate
/// 复用（它是 crate 私有常量），但两处的感知延迟必须一致 —— 否则同一个插话在
/// `bash` 里 100ms 生效、在 `subagent` 里 300ms 生效。
const STEER_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 同步派发的两种结局。
enum Dispatch {
    /// 子 agent 跑完了。
    Done(Result<SubagentOutcome, String>),
    /// 用户插话 ⇒ 放弃等待（drop 掉 runner future = 取消子 agent）。
    Steered,
}

/// 跑一次子 agent，并同时监视父边界源上的**用户插话**。
///
/// **为什么放弃等待就够**：`runner.run(..)` 的 future 被 drop ⇒ 子 agent 的回合
/// 随之中止 ⇒ 它自己的 bash 靠 `kill_on_drop` 收尸（与「父回合被中止」同一条路径）。
/// 插话本身**不在这里消费**：它仍排在边界队列里，由 `BasicLoop` 在轮边界 `take`。
///
/// **`boundary` 为 `None`**（`-p` / `--json` 无 steering 生产者）⇒ 退化为单纯等待，
/// 与「没有探针」逐字等价。
async fn run_or_steer(
    runner: &dyn SubagentRunner,
    prompt: &str,
    model: Option<&str>,
    boundary: Option<&dyn BoundarySource>,
) -> Dispatch {
    let wait_steer = async {
        match boundary {
            // 先看一眼：插话已到时不白白发起一次派发。
            Some(src) => loop {
                if src.has_pending_steer() {
                    return;
                }
                tokio::time::sleep(STEER_POLL_INTERVAL).await;
            },
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        biased;
        out = runner.run(prompt, model, boundary) => Dispatch::Done(out),
        _ = wait_steer => Dispatch::Steered,
    }
}

/// 因用户插话放弃等待时的工具结果。
///
/// **`is_error: false`**：派发没坏，是用户改了主意 —— 报成失败会让模型去「重试」，
/// 而用户想要的恰恰是别再跑了。
fn steered_dispatch_result() -> ToolResult {
    ToolResult {
        content: "[subagent: dispatch aborted early because the user sent a message while the \
                  sub-agent was working. The sub-agent was cancelled and did NOT finish — its \
                  partial work is not available. That message will enter your context as soon as \
                  this batch of tool calls finishes; read it before deciding whether to re-dispatch.]"
            .to_string(),
        is_error: false,
    }
}

/// 派发工具：schema `{description, prompt, [model]}`，把调用转给 `ToolContext.subagent`。
pub struct SubagentTool {
    /// 外闸 = 子预算 + 宽限（ADO-0029 修正 2）。
    outer_timeout: Duration,
}

impl SubagentTool {
    pub fn new(outer_timeout: Duration) -> Self {
        Self { outer_timeout }
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "subagent",
            "Delegate a self-contained task to an isolated sub-agent and get back a summary plus a transcript path",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "description": {"type": "string", "description": "3-5 word label for the user (shown in the tool line)"},
                    "prompt": {"type": "string", "description": "Complete self-contained task; the sub-agent does NOT see this conversation"},
                    "model": {"type": "string", "description": "Optional model override"}
                },
                "required": ["description", "prompt"]
            }),
        )
    }

    fn timeout(&self) -> Option<Duration> {
        Some(self.outer_timeout)
    }

    /// 派发工具与同批的其他 `subagent` 调用并发执行（同一批 **每一个** 都声明
    /// `true` 才走并行；混合批次自动退回串行）。子 agent 各有独立的会话 / 事件出口 /
    /// 边界源，「写目标不相交」由 guideline 约束（本仓无沙箱）。
    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("`prompt` 必填".into()))?;
        let model = input.get("model").and_then(|v| v.as_str());
        let runner = ctx
            .subagent
            .ok_or_else(|| ToolError::Execution("本回合未挂载 subagent 能力".into()))?;
        let out = match run_or_steer(runner, prompt, model, ctx.boundary).await {
            Dispatch::Done(out) => out.map_err(ToolError::Execution)?,
            // 插话：放弃等待（子 agent 已取消），回合继续。
            Dispatch::Steered => return Ok(steered_dispatch_result()),
        };

        let mut content = String::new();
        if let Some(d) = &out.diagnostic {
            content.push_str(d);
        }
        if !out.summary.is_empty() {
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(&out.summary);
        }
        if let Some(a) = &out.artifact {
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(&format!("[full transcript: {a}]"));
        }
        Ok(ToolResult {
            content,
            is_error: out.stop_reason != StopReason::Completed,
        })
    }
}

/// **后台**派发工具：立刻返回句柄，子 agent 在后台跑；**结果会自动注入会话**。
///
/// 存在的理由（实测，会话 `1792579023_125340094`）：同步派发把父回合钉死 ——
/// 14 次派发累计阻塞 **3,605 s**，最长一次 **965 s**；而 turn 16 的三次派发
/// 分在**三轮**里（965 / 511 / 355 s）⇒ 串行 1,831 s，同一批只需 965 s。
///
/// **不需要 poll**：完成时结果作为 `Boundary::Deferred` 注入，`BasicLoop` 在下一个
/// 轮边界 append 进会话 ⇒ 模型不必记句柄、也不可能「忘记取」。
pub struct SubagentAsyncTool;

impl SubagentAsyncTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SubagentAsyncTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SubagentAsyncTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "subagent_async",
            "Start a sub-agent in the background and get a handle back immediately; it keeps \
             working while you do other things, and its result is delivered to you \
             automatically when it finishes — you do NOT need to poll. Prefer this over \
             `subagent` when you want to dispatch several sub-tasks at once, or when you want \
             to keep answering the user while the sub-agent works",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "description": {"type": "string", "description": "3-5 word label for the user (shown in the tool line)"},
                    "prompt": {"type": "string", "description": "Complete self-contained task; the sub-agent does NOT see this conversation"},
                    "model": {"type": "string", "description": "Optional model override"}
                },
                "required": ["description", "prompt"]
            }),
        )
    }

    /// 只登记后台任务（不跑子 agent）⇒ 与其它只读工具一样可并发。
    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("`prompt` 必填".into()))?;
        let model = input.get("model").and_then(|v| v.as_str());
        let runner = ctx
            .subagent
            .ok_or_else(|| ToolError::Execution("本回合未挂载 subagent 能力".into()))?;
        let handle = runner
            .spawn(prompt, model, ctx.boundary)
            .map_err(ToolError::Execution)?;
        Ok(ToolResult {
            content: format!(
                "Started sub-agent in the background.\nhandle: {handle}\n\
                 Its result will be delivered to you automatically when it finishes — do NOT \
                 poll for it. Keep working on your own part now. It is cancelled if this turn \
                 ends before it finishes."
            ),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use ys_core::Role;

    #[test]
    fn child_boundary_takes_own_and_probes_parent() {
        let parent = Arc::new(QueueBoundarySource::new());
        let cb = ChildBoundary::new(Some(parent.clone()));
        assert!(!cb.is_aborted());
        assert!(cb.take().is_none(), "空队列");
        // 父 abort ⇒ 子探针可见。
        parent.push(Boundary::Abort);
        assert!(cb.is_aborted());
        // 子自己的 abort（watchdog）也能被 take 交还。
        cb.own().push(Boundary::Abort);
        assert_eq!(cb.take(), Some(Boundary::Abort));
        // 父队列不被子消费（父仍能自己 take）。
        assert_eq!(parent.take(), Some(Boundary::Abort));
    }

    /// 构造一条纯文本 `User` 消息（投递表的操作对象）。
    fn user_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    /// 投递表的三种状态：已绑定 ⇒ 直接进队列；未绑定 ⇒ 留表；绑定后补投。
    ///
    /// 变异：① `deliver` 在未绑定时直接丢弃 ⇒ 第 2 段变红；
    ///       ② `deliver` 忽略落点恒留表 ⇒ 第 1 段变红。
    #[test]
    fn pending_results_delivers_when_bound_and_queues_when_not() {
        let results = Arc::new(PendingResults::default());
        let sink = Arc::new(QueueBoundarySource::new());

        // ① 未绑定 ⇒ 留表，边界源看不到。
        assert!(!results.deliver("sa1".into(), user_msg("早到")));
        assert_eq!(results.pending_len(), 1);
        assert!(sink.take().is_none(), "未绑定时不得进任何队列");

        // ② 绑定后 ⇒ 直接进队列，不再留表。
        let _binding = results.bind(sink.clone());
        assert!(results.deliver("sa2".into(), user_msg("在位")));
        assert_eq!(results.pending_len(), 1, "留表的仍只有那条早到的");
        match sink.take() {
            Some(Boundary::Deferred { handle, .. }) => assert_eq!(handle, "sa2"),
            other => panic!("应取到 sa2 的 Deferred，得到 {other:?}"),
        }

        // ③ 补投：把留表的取走，之后再取为空（不会重复投递）。
        let pending = results.take_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, "sa1");
        assert!(results.take_pending().is_empty(), "取走后不得重复投递");
    }

    /// 解绑只在**代数仍相同**时生效：晚 drop 的旧守卫不得抹掉新回合的落点。
    ///
    /// 变异：`unbind` 不看代数（恒清空）⇒ 第二个守卫 drop 后新落点丢失 ⇒ 变红。
    #[test]
    fn pending_results_stale_binding_does_not_clobber_newer_one() {
        let results = Arc::new(PendingResults::default());
        let first = Arc::new(QueueBoundarySource::new());
        let second = Arc::new(QueueBoundarySource::new());

        let old = results.bind(first.clone());
        let _new = results.bind(second.clone());
        // 旧守卫**晚于**新绑定才 drop（顺序被调度打乱时的真实情形）。
        drop(old);

        assert!(
            results.deliver("sa1".into(), user_msg("x")),
            "旧守卫 drop 不得清掉新落点"
        );
        assert!(second.take().is_some(), "结果应进最新的落点");
        assert!(first.take().is_none(), "旧落点不该拿到结果");
    }

    #[test]
    fn usage_totals_accumulate_and_drain() {
        let t = UsageTotals::default();
        let u = Usage {
            input_tokens: 3,
            output_tokens: 2,
            prompt_cache_hit_tokens: Some(1),
            prompt_cache_miss_tokens: None,
            reasoning_tokens: None,
        };
        t.add(&u, u.cache_split());
        let u = Usage {
            input_tokens: 4,
            output_tokens: 5,
            prompt_cache_hit_tokens: Some(6),
            prompt_cache_miss_tokens: None,
            reasoning_tokens: None,
        };
        t.add(&u, u.cache_split());
        let (d, cache) = t.drain();
        assert_eq!((d.input_tokens, d.output_tokens), (7, 7));
        assert_eq!(d.prompt_cache_hit_tokens, Some(7), "只累加上报过的轮");
        assert_eq!(
            d.prompt_cache_miss_tokens, None,
            "`miss` 恒 `None`：由 `input − hit` 推导，不落地"
        );
        // 该次调用 miss = saturating_sub(input − hit)：
        //   (input=3, hit=1) ⇒ miss=2 ；(input=4, hit=6) ⇒ miss=0（provider 异常，饱和）
        // 故累加结果是 (1+6, 2+0) = (7, 2)。
        // 若从聚合 input 派生（7 − 7 = 0）会错成 (7, 0)。
        assert_eq!(
            cache,
            Some((7, 2)),
            "上卷的缓存读数必须是逐次累加的 (hit, miss)，不能从聚合 input 派生"
        );
        assert_eq!(t.drain().0.input_tokens, 0, "drain 后清零");
    }

    /// **M8 变异测试**：混合上报时 `(hit, miss)` 必须逐次累加，不能从聚合 `input` 派生。
    ///
    /// 构造：子调用 A `(input=1000, hit=900)`、B `(input=1000, hit=None)`。
    /// - 正确（逐次累加）：`(900, 100)` ⇒ 命中率 90%
    /// - 若从聚合 `input` 派生：`input=2000, hit=900` ⇒ `(900, 1100)` ⇒ 命中率 45%
    ///
    /// 变异：把 `drain` 的第二个返回值改成 `usage.cache_split()`
    /// （即从聚合 `Usage` 派生）⇒ 本用例从 `(900,100)` 变 `(900,1100)` ⇒ 变红。
    #[test]
    fn usage_totals_cache_readings_accumulate_per_call() {
        let t = UsageTotals::default();
        let u = Usage {
            input_tokens: 1000,
            prompt_cache_hit_tokens: Some(900),
            ..Default::default()
        };
        t.add(&u, u.cache_split());
        let u = Usage {
            input_tokens: 1000,
            ..Default::default() // hit = None：未上报
        };
        t.add(&u, u.cache_split());
        let (usage, cache) = t.drain();
        assert_eq!(usage.input_tokens, 2000, "input 对未上报的子调用也累加");
        assert_eq!(
            cache,
            Some((900, 100)),
            "必须逐次累加：未上报的子调用不得把其 input 算成未命中"
        );
    }

    /// 子调用全都不报缓存命中 ⇒ 上卷为 `None`（未上报，不得伪造 0）。
    ///
    /// 变异说明：把 `drain` 的 `prompt_cache_hit_tokens` 改成恒 `Some(hit)` ⇒ 本测试变红。
    #[test]
    fn usage_totals_cache_hit_none_when_unreported() {
        let t = UsageTotals::default();
        let u = Usage {
            input_tokens: 10,
            ..Default::default()
        };
        t.add(&u, u.cache_split());
        assert_eq!(t.drain().0.prompt_cache_hit_tokens, None);
    }

    /// 有一个子调用报 `Some(0)` ⇒ 上卷为 `Some(0)`（真 0 不能折叠成 `None`）。
    ///
    /// 变异说明：`add` 里按 `v > 0` 而非 `Some(_)` 置位 ⇒ 本测试变红；
    /// `drain` 里写成 `(hit > 0).then_some(...)` ⇒ 本测试同样变红。
    #[test]
    fn usage_totals_cache_hit_keeps_reported_zero() {
        let t = UsageTotals::default();
        let u = Usage {
            prompt_cache_hit_tokens: Some(0),
            ..Default::default()
        };
        t.add(&u, u.cache_split());
        assert_eq!(t.drain().0.prompt_cache_hit_tokens, Some(0));
    }

    /// 子调用全都不报 reasoning ⇒ 上卷为 `None`（未上报，不得伪造 0）。
    ///
    /// 变异说明：把 `drain` 的 `reasoning_tokens` 改成恒 `Some(reasoning)` ⇒ 本测试变红。
    #[test]
    fn usage_totals_reasoning_none_when_unreported() {
        let t = UsageTotals::default();
        let u = Usage {
            input_tokens: 1,
            ..Default::default()
        };
        t.add(&u, u.cache_split());
        let u = Usage {
            input_tokens: 2,
            ..Default::default()
        };
        t.add(&u, u.cache_split());
        assert_eq!(t.drain().0.reasoning_tokens, None);
    }

    /// 有一个子调用报 `Some(0)` ⇒ 上卷为 `Some(0)`（真 0 不能折叠成 `None`）。
    ///
    /// 变异说明：`add` 里按 `v > 0` 而非 `Some(_)` 置位 ⇒ 本测试变红；
    /// `drain` 里写成 `(reasoning > 0).then_some(...)` ⇒ 本测试同样变红。
    #[test]
    fn usage_totals_reasoning_keeps_reported_zero() {
        let t = UsageTotals::default();
        let u = Usage {
            reasoning_tokens: Some(0),
            ..Default::default()
        };
        t.add(&u, u.cache_split());
        assert_eq!(t.drain().0.reasoning_tokens, Some(0));
    }

    /// 多次上报 ⇒ 相加；`None` 不影响已上报的累计。
    ///
    /// 变异说明：把 `add` 的 `reasoning` 累加删掉 / 恒置 0 ⇒ 断言变红；
    /// 把 `drain` 硬写 `None`（修复前的行为）⇒ 断言同样变红。
    #[test]
    fn usage_totals_reasoning_sums_reported_values() {
        let t = UsageTotals::default();
        let u = Usage {
            reasoning_tokens: Some(3),
            ..Default::default()
        };
        t.add(&u, u.cache_split());
        let u = Usage {
            reasoning_tokens: None,
            ..Default::default()
        };
        t.add(&u, u.cache_split());
        let u = Usage {
            reasoning_tokens: Some(4),
            ..Default::default()
        };
        t.add(&u, u.cache_split());
        assert_eq!(t.drain().0.reasoning_tokens, Some(7));
        // drain 后清零，且「是否上报」也复位。
        assert_eq!(t.drain().0.reasoning_tokens, None);
    }

    #[test]
    fn truncate_bytes_is_utf8_safe() {
        let s = "中".repeat(100); // 300 bytes
        let out = truncate_bytes(&s, 10);
        assert!(out.len() <= 10 + "\n…[truncated]".len());
        assert!(out.starts_with('中'));
    }

    /// 产物节的格式保证：子不报 ⇒ 补 `(none)`；子报了 ⇒ **原样保留**（不重复）。
    #[test]
    fn files_section_is_guaranteed_and_not_duplicated() {
        let bare = ensure_files_section("结论");
        assert!(bare.ends_with("## Files\n(none)"), "{bare}");

        let reported = "结论\n\n## Files\n- src/a.rs";
        assert_eq!(
            ensure_files_section(reported),
            reported,
            "子自己报了就不补（避免出现两段）"
        );
    }

    /// 工具把 runner 的 outcome 组装成 `ToolResult`；非 `Completed` ⇒ `is_error`。
    #[tokio::test]
    async fn subagent_tool_maps_outcome_to_result() {
        struct FakeRunner(StopReason);
        #[async_trait]
        impl SubagentRunner for FakeRunner {
            async fn run(
                &self,
                prompt: &str,
                _model: Option<&str>,
                _boundary: Option<&dyn BoundarySource>,
            ) -> Result<SubagentOutcome, String> {
                Ok(SubagentOutcome {
                    summary: format!("摘要:{prompt}"),
                    artifact: Some("/tmp/t.md".into()),
                    stop_reason: self.0.clone(),
                    diagnostic: (self.0 != StopReason::Completed)
                        .then(|| "[diagnostic: stop_reason=max-rounds elapsed_ms=1]".into()),
                    usage: Usage::default(),
                })
            }
        }

        let tool = SubagentTool::new(Duration::from_secs(60));
        let runner = FakeRunner(StopReason::Completed);
        let ctx =
            ToolContext::new(None, PathBuf::from("."), PathBuf::from(".")).with_subagent(&runner);
        let r = tool
            .call(serde_json::json!({"description":"d","prompt":"干活"}), ctx)
            .await
            .unwrap();
        assert!(!r.is_error);
        assert!(r.content.contains("摘要:干活"));
        assert!(r.content.contains("[full transcript: /tmp/t.md]"));

        let runner2 = FakeRunner(StopReason::MaxRounds);
        let ctx2 =
            ToolContext::new(None, PathBuf::from("."), PathBuf::from(".")).with_subagent(&runner2);
        let r2 = tool
            .call(serde_json::json!({"description":"d","prompt":"干活"}), ctx2)
            .await
            .unwrap();
        assert!(r2.is_error, "非 Completed ⇒ is_error（父回合不失败）");
        assert!(
            r2.content.starts_with("[diagnostic:"),
            "诊断必须是正文首行：{}",
            r2.content
        );
        let diagnostic_at = r2.content.find("[diagnostic:").unwrap();
        let summary_at = r2.content.find("摘要:干活").unwrap();
        let path_at = r2.content.find("[full transcript:").unwrap();
        assert!(diagnostic_at < summary_at && summary_at < path_at);

        // 未挂载 ⇒ Execution 错误。
        let bare = ToolContext::new(None, PathBuf::from("."), PathBuf::from("."));
        let err = tool
            .call(serde_json::json!({"description":"d","prompt":"x"}), bare)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    /// 同步派发时用户插话 ⇒ **放弃等待**（子 agent 被取消），且**不消费插话**。
    ///
    /// 用「永不返回的 runner」作替身：不插话就永远不返回 ⇒ 本用例只能靠探针收场。
    /// 三条不变量：
    /// ① 插话后**很快**返回（不把父回合钉到子任务跑完）；
    /// ② 结果是 `is_error: false` 且如实说明「派发已中止、子 agent 未完成」；
    /// ③ 插话**仍在队列里**（工具不得偷吃，只能由轮边界 `take`）。
    ///
    /// 变异：删掉 `run_or_steer` 里的 `wait_steer` 分支 ⇒ 永不返回 ⇒ 守卫超时变红。
    #[tokio::test]
    async fn subagent_tool_steer_abandons_dispatch_without_consuming_steer() {
        struct HangingRunner;
        #[async_trait]
        impl SubagentRunner for HangingRunner {
            async fn run(
                &self,
                _prompt: &str,
                _model: Option<&str>,
                _boundary: Option<&dyn BoundarySource>,
            ) -> Result<SubagentOutcome, String> {
                std::future::pending::<()>().await;
                unreachable!("永不返回")
            }
        }

        let boundary = Arc::new(QueueBoundarySource::new());
        let steer = Boundary::Steer(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "先停，我要改需求".into(),
            }],
        });
        let producer = Arc::clone(&boundary);
        let steer_for_task = steer.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            producer.push(steer_for_task);
        });

        let tool = SubagentTool::new(Duration::from_secs(600));
        let runner = HangingRunner;
        let ctx = ToolContext::new(Some(&*boundary), PathBuf::from("."), PathBuf::from("."))
            .with_subagent(&runner);

        let started = Instant::now();
        let r = tokio::time::timeout(
            Duration::from_secs(5),
            tool.call(serde_json::json!({"description":"d","prompt":"干活"}), ctx),
        )
        .await
        .expect("插话必须能放弃等待（否则父回合被子任务钉死）")
        .expect("放弃等待不是工具错误");
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(3),
            "插话应在一个轮询周期级别内收口，实际 {elapsed:?}"
        );
        assert!(!r.is_error, "插话中途放弃不是失败（否则模型会去重试）");
        assert!(
            r.content.contains("aborted early") && r.content.contains("user sent a message"),
            "结果须说清是插话导致的提前中止：{}",
            r.content
        );
        assert!(
            r.content.contains("did NOT finish"),
            "须明确子 agent 未完成（别让模型以为拿到了结果）：{}",
            r.content
        );

        // 插话未被消费。
        assert_eq!(boundary.len(), 1, "工具不得消费用户的插话");
        assert!(boundary.has_pending_steer());
        assert_eq!(boundary.take(), Some(steer));
    }

    /// 无边界源（`-p` / `--json`）时 `run_or_steer` 退化为单纯等待：正常派发不受影响。
    #[tokio::test]
    async fn run_or_steer_without_boundary_is_plain_dispatch() {
        struct CompletingRunner;
        #[async_trait]
        impl SubagentRunner for CompletingRunner {
            async fn run(
                &self,
                prompt: &str,
                _model: Option<&str>,
                _boundary: Option<&dyn BoundarySource>,
            ) -> Result<SubagentOutcome, String> {
                Ok(SubagentOutcome {
                    summary: format!("摘要:{prompt}"),
                    artifact: None,
                    stop_reason: StopReason::Completed,
                    diagnostic: None,
                    usage: Usage::default(),
                })
            }
        }

        match run_or_steer(&CompletingRunner, "干活", None, None).await {
            Dispatch::Done(Ok(out)) => assert_eq!(out.summary, "摘要:干活"),
            _ => panic!("无边界源时应正常派发并返回结果"),
        }
    }

    #[test]
    fn subagent_tool_requires_prompt_and_declares_timeout() {
        let tool = SubagentTool::new(Duration::from_secs(123));
        assert_eq!(tool.timeout(), Some(Duration::from_secs(123)));
        assert_eq!(tool.spec().name, "subagent");
    }

    /// 端到端：`AppSubagentRunner` 用一个 `MockModel` 跑一个真子 agent（独立会话），
    /// 断言摘要来自子 agent、usage 上卷、转录落盘。
    #[tokio::test]
    async fn app_runner_runs_isolated_child_and_persists_transcript() {
        use ys_model::{FinishReason, ModelError, ModelEventSink, ModelRequest, ModelResponse};
        use ys_tools_web::FetchedDir;

        /// 固定 usage 的模型：`MockModel` 恒报 0，测不出「usage 上卷」这条接线。
        struct FixedUsageModel;
        #[async_trait]
        impl Model for FixedUsageModel {
            fn model_id(&self) -> &str {
                "fixed"
            }
            async fn complete(
                &self,
                _request: ModelRequest,
                _sink: &mut dyn ModelEventSink,
            ) -> Result<ModelResponse, ModelError> {
                Ok(ModelResponse {
                    message: ys_core::Message {
                        role: ys_core::Role::Assistant,
                        content: vec![ys_core::ContentBlock::Text {
                            text: "子 agent 的结论".into(),
                        }],
                    },
                    usage: Usage {
                        input_tokens: 11,
                        output_tokens: 7,
                        prompt_cache_hit_tokens: Some(9),
                        prompt_cache_miss_tokens: None,
                        reasoning_tokens: None,
                    },
                    finish_reason: FinishReason::Stop,
                })
            }
        }

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base =
            std::env::temp_dir().join(format!("yushan_subagent_{}_{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let model = FixedUsageModel;

        let dir = Arc::new(SubagentsDir::at(base.join("subagents"), false));
        let web = WebConfig::offline(FetchedDir::Temp(base.join("fetched")));
        let totals = Arc::new(UsageTotals::default());
        let runner = AppSubagentRunner::new(
            Some(Arc::new(model)),
            None,
            Arc::new(Mutex::new(HashMap::new())),
            "你是子 agent".to_string(),
            RunLimits::new(3),
            Duration::from_secs(30),
            base.clone(),
            web,
            dir.clone(),
            totals.clone(),
            None,
            Arc::new(PendingResults::default()),
            Arc::new(LiveSubagents::default()),
        );

        let out = runner.run("查 X", None, None).await.unwrap();
        assert_eq!(out.stop_reason, StopReason::Completed);
        assert!(
            out.summary.contains("子 agent 的结论"),
            "摘要 = {}",
            out.summary
        );
        assert!(
            out.summary.contains("## Files"),
            "runner 必须保证产物节存在（子没报 ⇒ 写 (none)）：{}",
            out.summary
        );
        let artifact = out.artifact.expect("应落盘转录");
        assert!(
            std::path::Path::new(&artifact).exists(),
            "转录文件应存在: {artifact}"
        );
        let (up, up_cache) = totals.drain();
        assert_eq!((up.input_tokens, up.output_tokens), (11, 7), "usage 应上卷");
        assert_eq!(up.prompt_cache_hit_tokens, Some(9), "缓存读数也应上卷");
        assert_eq!(
            up.prompt_cache_miss_tokens, None,
            "`miss` 不再落地，由 `input − hit` 推导"
        );
        assert_eq!(up_cache, Some((9, 2)), "上卷的 (hit, miss) 逐次累加");

        // fail-closed：指定 model 名 ⇒ Err（不静默回退）。
        let err = runner
            .run("x", Some("other-model"), None)
            .await
            .unwrap_err();
        assert!(err.contains("fail-closed"));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 派发总量上限：超过后返回响亮的 Err。
    #[tokio::test]
    async fn app_runner_enforces_per_turn_dispatch_cap() {
        use ys_model::MockModel;
        use ys_tools_web::FetchedDir;

        static COUNTER: AtomicU64 = AtomicU64::new(1);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base =
            std::env::temp_dir().join(format!("yushan_subagent_cap_{}_{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let model = MockModel::new("m");
        for _ in 0..(DEFAULT_MAX_DISPATCHES_PER_TURN + 1) {
            model.push_text("ok");
        }
        let web = WebConfig::offline(FetchedDir::Temp(base.join("fetched")));
        let runner = AppSubagentRunner::new(
            Some(Arc::new(model)),
            None,
            Arc::new(Mutex::new(HashMap::new())),
            String::new(),
            RunLimits::new(2),
            Duration::from_secs(30),
            base.clone(),
            web,
            Arc::new(SubagentsDir::at(base.join("subagents"), false)),
            Arc::new(UsageTotals::default()),
            None,
            Arc::new(PendingResults::default()),
            Arc::new(LiveSubagents::default()),
        );
        for _ in 0..DEFAULT_MAX_DISPATCHES_PER_TURN {
            runner.run("t", None, None).await.unwrap();
        }
        let err = runner.run("t", None, None).await.unwrap_err();
        assert!(err.contains("上限"), "超限应响亮报错: {err}");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// **P0**：具名模型缓存的键必须包含**有效推理强度**。
    ///
    /// 同一模型名、有效强度 `high`（配置默认）与覆盖为 `max` 两次派发 ⇒ factory
    /// 被调用两次、缓存两条、两次摘要来自各自档位构建的实例。
    ///
    /// 变异：把 `SubagentModelCache` 的键改回 `String`（只用名字）⇒ 第二次派发命中旧实例，
    /// factory 只调用一次 ⇒ `seen.len()` 与第二条摘要断言变红。
    #[tokio::test]
    async fn named_model_cache_key_includes_effort() {
        use ys_model::{FinishReason, ModelError, ModelEventSink, ModelRequest, ModelResponse};
        use ys_tools_web::FetchedDir;

        /// 把构建时的档位编进 `model_id` 与回复，便于从摘要观察实际用了哪个实例。
        struct EffortEchoModel(String);
        #[async_trait]
        impl Model for EffortEchoModel {
            fn model_id(&self) -> &str {
                &self.0
            }
            async fn complete(
                &self,
                _request: ModelRequest,
                _sink: &mut dyn ModelEventSink,
            ) -> Result<ModelResponse, ModelError> {
                Ok(ModelResponse {
                    message: ys_core::Message {
                        role: ys_core::Role::Assistant,
                        content: vec![ys_core::ContentBlock::Text {
                            text: format!("echo:{}", self.0),
                        }],
                    },
                    usage: Usage::default(),
                    finish_reason: FinishReason::Stop,
                })
            }
        }

        static COUNTER: AtomicU64 = AtomicU64::new(2);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "yushan_subagent_effort_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let mut config = {
            let _guard = crate::test_env::env_lock();
            crate::config::Config::from_env().unwrap()
        };
        config.api_base = Some("https://example.test".into());
        config.api_key = Some("sk-test".into());
        type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        config.set_model_factory_for(move |cfg, name| {
            let effort = cfg.effective_reasoning_effort();
            recorder
                .lock()
                .unwrap()
                .push((name.to_string(), effort.clone()));
            let tag = format!("{name}:{}", effort.unwrap_or_else(|| "none".into()));
            Some(Box::new(EffortEchoModel(tag)) as Box<dyn Model>)
        });

        let cache: SubagentModelCache = SubagentModelCache::default();
        let dir = || Arc::new(SubagentsDir::at(base.join("subagents"), false));
        let web = || WebConfig::offline(FetchedDir::Temp(base.join("fetched")));

        // 第一次派发：无会话覆盖 ⇒ 用配置默认 `high`。
        let first = {
            let runner = AppSubagentRunner::new(
                None,
                Some(&config),
                cache.clone(),
                "child".to_string(),
                RunLimits::new(2),
                Duration::from_secs(30),
                base.clone(),
                web(),
                dir(),
                Arc::new(UsageTotals::default()),
                None,
                Arc::new(PendingResults::default()),
                Arc::new(LiveSubagents::default()),
            );
            runner.run("x", Some("shared-name"), None).await.unwrap()
        };
        assert!(
            first.summary.contains("echo:shared-name:high"),
            "第一次派发应使用配置默认档位的实例：{}",
            first.summary
        );

        // 用户 `/effort max` 后同名再派发：必须重建，而非命中旧实例。
        config.effort = Some(ys_core::EffortLevel::Max);
        let second = {
            let runner = AppSubagentRunner::new(
                None,
                Some(&config),
                cache.clone(),
                "child".to_string(),
                RunLimits::new(2),
                Duration::from_secs(30),
                base.clone(),
                web(),
                dir(),
                Arc::new(UsageTotals::default()),
                None,
                Arc::new(PendingResults::default()),
                Arc::new(LiveSubagents::default()),
            );
            runner.run("x", Some("shared-name"), None).await.unwrap()
        };
        assert!(
            second.summary.contains("echo:shared-name:max"),
            "档位变化后必须用新档位实例：{}",
            second.summary
        );

        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                ("shared-name".to_string(), Some("high".to_string())),
                ("shared-name".to_string(), Some("max".to_string())),
            ],
            "factory 必须按（名字, 有效强度）各构建一次"
        );
        assert_eq!(cache.lock().unwrap().len(), 2, "缓存应有两条档位不同的条目");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 子 agent 内部会话独立：`Role` 齐备且不含父历史。
    #[test]
    fn transcript_body_renders_roles() {
        let s = MemorySession::new();
        // 空会话 → 空正文（不 panic）。
        let body = transcript_body(&s, &HashMap::new(), &StopReason::Completed, None, 0, None);
        assert!(
            body.starts_with("## Run\nstop_reason=completed\nelapsed_ms=0\n"),
            "{body}"
        );
        let _ = Role::User;
    }

    #[test]
    fn cancel_cause_prioritizes_parent_and_only_marks_cancelled() {
        assert_eq!(
            cancel_cause(&StopReason::Cancelled, false, false),
            Some(CancelCause::Unknown)
        );
        assert_eq!(
            cancel_cause(&StopReason::Cancelled, false, true),
            Some(CancelCause::ChildBudget)
        );
        assert_eq!(
            cancel_cause(&StopReason::Cancelled, true, true),
            Some(CancelCause::ParentTurnCancelled),
            "父取消优先于同时到点的子预算"
        );
        assert_eq!(cancel_cause(&StopReason::MaxRounds, true, true), None);
        assert_eq!(cancel_cause(&StopReason::Completed, false, false), None);
    }

    #[test]
    fn diagnostic_is_single_line_bounded_and_utf8_safe() {
        let clean = render_diagnostic("stop_reason=cancelled\r\ncause=x\u{7}\telapsed_ms=12");
        assert!(clean.starts_with(DIAGNOSTIC_PREFIX), "{clean}");
        assert!(clean.ends_with(']'), "{clean}");
        assert!(
            !clean.chars().any(char::is_control),
            "控制字符必须替换成空格：{clean:?}"
        );
        assert!(clean.contains("stop_reason=cancelled"), "{clean:?}");
        assert!(clean.contains("cause=x"), "{clean:?}");
        assert!(clean.contains("elapsed_ms=12"), "{clean:?}");

        let long = format!("stop_reason=cancelled error={}", "中".repeat(600));
        let truncated = render_diagnostic(&long);
        assert!(
            truncated.starts_with(DIAGNOSTIC_PREFIX),
            "截断后仍须保留前缀"
        );
        assert!(
            truncated.len() <= MAX_DIAGNOSTIC_BYTES,
            "诊断 {} bytes 超上限",
            truncated.len()
        );
        assert!(truncated.ends_with("..."), "截断标记必须是 ASCII ...");
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn run_diagnostic_has_cause_only_for_cancelled() {
        let cancelled = run_diagnostic(
            &StopReason::Cancelled,
            Some(CancelCause::ParentTurnCancelled),
            17,
            &[],
        )
        .unwrap();
        assert!(cancelled.contains("stop_reason=cancelled"));
        assert!(cancelled.contains("cause=parent-turn-cancelled"));
        assert!(cancelled.contains("elapsed_ms=17"));

        let max_rounds = run_diagnostic(&StopReason::MaxRounds, None, 23, &[]).unwrap();
        assert!(max_rounds.contains("stop_reason=max-rounds"));
        assert!(!max_rounds.contains("cause="));
        assert!(max_rounds.contains("elapsed_ms=23"));
        assert_eq!(run_diagnostic(&StopReason::Completed, None, 1, &[]), None);
    }

    /// 故障（provider 错误）**不得**被归成取消 —— 这是 `Failed` 变体存在的理由。
    ///
    /// 事故背景：一次长会话的 provider 流中断导致子 agent 整段失败，
    /// 诊断行却写成 `stop_reason=cancelled cause=unknown error=...SSE stream...`
    /// —— 自相矛盾，且把真故障埋在「用户取消」里。
    ///
    /// 变异：把 `stop_reason_label` 里的 `Failed => "failed"` 删掉 ⇒ 第一条断言变红；
    /// 让 `cancel_cause` 对 `Failed` 也返回 `Some(..)` ⇒ 第二条断言变红。
    #[test]
    fn failed_is_not_reported_as_cancellation() {
        let failed = run_diagnostic(
            &StopReason::Failed,
            None,
            341_923,
            &["error=model provider error: SSE stream error".to_string()],
        )
        .expect("故障必须有诊断");

        assert!(
            failed.contains("stop_reason=failed"),
            "故障必须标成 failed，实际: {failed}"
        );
        assert!(
            !failed.contains("stop_reason=cancelled"),
            "故障不得伪装成取消，实际: {failed}"
        );
        assert!(
            !failed.contains("cause="),
            "故障不是取消 ⇒ 不得有取消归因，实际: {failed}"
        );
        // 错误文本必须保留 —— 它是追查的唯一线索。
        assert!(failed.contains("SSE stream error"), "实际: {failed}");

        // 归因函数本身也必须守住这条：非取消一律 `None`。
        for cancelled in [false, true] {
            assert_eq!(
                cancel_cause(&StopReason::Failed, cancelled, cancelled),
                None,
                "Failed 在任何取消状态下都不得有 cause"
            );
        }
        // 对照：真取消仍有归因（防止上面的断言被“恒 None”洗白）。
        assert_eq!(
            cancel_cause(&StopReason::Cancelled, true, false),
            Some(CancelCause::ParentTurnCancelled)
        );
    }

    /// **端到端锁住 `Err` 分支**：model 返回 `Provider` 错误时，`ChildTask::execute`
    /// 必须以 `Failed` 收场、**不带**取消归因，且错误文本仍在诊断里。
    ///
    /// 为何必须走真实路径：`failed_is_not_reported_as_cancellation` 直接构造
    /// `StopReason::Failed`，**抓不到**「`Err` 分支写回 `Cancelled`」这个变异
    /// （实测过：那次变异让它全绿）。本用例从 `runner.run` 进去，必经 `Err` 分支。
    ///
    /// 变异：把 `Err` 分支的 `StopReason::Failed` 改回 `Cancelled`
    /// （并把 `cause` 改回 `Some(CancelCause::Unknown)`）⇒ 本用例变红。
    #[tokio::test]
    async fn provider_error_surfaces_as_failed_not_cancelled() {
        use ys_model::{ModelError, ModelEventSink, ModelRequest, ModelResponse};
        use ys_tools_web::FetchedDir;

        /// 恒返回 provider 错误 —— 模拟流中断（`reason_class=stream`）。
        struct FailingModel;
        #[async_trait]
        impl Model for FailingModel {
            fn model_id(&self) -> &str {
                "failing"
            }
            async fn complete(
                &self,
                _request: ModelRequest,
                _sink: &mut dyn ModelEventSink,
            ) -> Result<ModelResponse, ModelError> {
                Err(ModelError::Provider(
                    "SSE stream error: error decoding response body \
                     (streaming already began; not retried)"
                        .to_string(),
                ))
            }
        }

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "yushan_subagent_failed_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let dir = Arc::new(SubagentsDir::at(base.join("subagents"), false));
        let web = WebConfig::offline(FetchedDir::Temp(base.join("fetched")));
        let runner = AppSubagentRunner::new(
            Some(Arc::new(FailingModel)),
            None,
            Arc::new(Mutex::new(HashMap::new())),
            "你是子 agent".to_string(),
            RunLimits::new(3),
            Duration::from_secs(30),
            base.clone(),
            web,
            dir,
            Arc::new(UsageTotals::default()),
            None,
            Arc::new(PendingResults::default()),
            Arc::new(LiveSubagents::default()),
        );

        let out = runner.run("查 X", None, None).await.unwrap();
        assert_eq!(
            out.stop_reason,
            StopReason::Failed,
            "provider 错误必须归成 Failed"
        );

        let diag = out.diagnostic.expect("必须留诊断");
        assert!(
            diag.contains("stop_reason=failed"),
            "诊断必须标 failed，实际: {diag}"
        );
        assert!(
            !diag.contains("stop_reason=cancelled"),
            "不得伪装成取消，实际: {diag}"
        );
        assert!(
            !diag.contains("cause="),
            "故障不是取消 ⇒ 无取消归因，实际: {diag}"
        );
        assert!(
            diag.contains("SSE stream error"),
            "必须保留错误文本: {diag}"
        );

        // 失败时不得伪装成“有产出”。
        assert!(out.summary.is_empty(), "失败不得编摘要: {:?}", out.summary);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// **流中断重试真的救回了任务**：先断一次、再成功 ⇒ 最终 `Completed`。
    ///
    /// 事故背景：provider 在 200 + 正常 ttfb 之后把流中途截断（实测 6307 次
    /// attempt 中 4 次），子 agent 已跑了 100+ 次工具调用却整段作废。
    /// 子 agent 的输出不进用户屏幕 ⇒ 丢弃部分输出、重发同一请求是安全的。
    ///
    /// 变异：把 `RunLimits::stream_interrupt_retries` 的默认值改成非零（污染 TUI
    /// 行为）⇒ `stream_retry_default_is_off` 变红；把 `basic.rs` 的重试循环去掉
    /// ⇒ 本用例变红（`Provider` 错误直接冒到 `Err` 分支）。
    #[tokio::test]
    async fn stream_interrupt_is_retried_and_task_survives() {
        use ys_model::MockModel;

        let base = unique_subagent_test_dir("stream_retry");
        let model = MockModel::new("m");
        // 第一次断流（模拟 SSE 截断），第二次成功。
        model.push_error(
            "SSE stream error: error decoding response body \
             (streaming already began; not retried)",
        );
        model.push_text("重试后成功产出");

        let dir = Arc::new(SubagentsDir::at(base.join("artifacts"), false));
        // `new` 无条件开启重试 ⇒ 直接用它验证产品路径。
        let runner = test_runner(model, &base, dir, RunLimits::new(3));

        let out = runner.run("查 X", None, None).await.unwrap();
        assert_eq!(
            out.stop_reason,
            StopReason::Completed,
            "流中断应被重试救回，不能整段失败（diagnostic={:?}）",
            out.diagnostic
        );
        assert!(
            out.summary.contains("重试后成功产出"),
            "重试后的产出必须进摘要，实际: {}",
            out.summary
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 重试是**有界的**：连断两次（只有 1 次额度）⇒ 第二次如实失败。
    ///
    /// 防止「无限重试把持续故障放大成 token 黑洞」。
    ///
    /// 变异：把 `stream_interrupt_retries` 从 1 改成 5 ⇒ 本用例变红（第二次会被重试）。
    #[tokio::test]
    async fn stream_interrupt_retry_is_bounded() {
        use ys_model::MockModel;

        let base = unique_subagent_test_dir("stream_retry_bounded");
        let model = MockModel::new("m");
        model.push_error("SSE stream error: first");
        model.push_error("SSE stream error: second");
        model.push_text("不应到达这里");

        let dir = Arc::new(SubagentsDir::at(base.join("artifacts"), false));
        let runner = test_runner(model, &base, dir, RunLimits::new(3));

        let out = runner.run("查 X", None, None).await.unwrap();
        assert_eq!(
            out.stop_reason,
            StopReason::Failed,
            "额度用尽后必须如实失败，不能无限重试"
        );
        assert!(!out.summary.contains("不应到达这里"));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 默认**关闭** —— TUI / `-p` 的行为必须逐字节不变。
    ///
    /// 客户端重发已开始的流会让用户眼前的文本重复一遍，主会话不能开。
    ///
    /// 变异：把 `RunLimits::default()` 的 `stream_interrupt_retries` 改成 1 ⇒ 变红。
    #[test]
    fn stream_retry_default_is_off() {
        assert_eq!(
            RunLimits::default().stream_interrupt_retries,
            0,
            "主会话（TUI / -p）不得重启已开始的流"
        );
        assert_eq!(RunLimits::new(5).stream_interrupt_retries, 0);
    }

    #[tokio::test]
    async fn transcript_prefers_event_result_when_session_was_budget_elided() {
        use ys_session::Session;

        let id = ToolCallId("call-big".into());
        let original = (0..60)
            .map(|i| {
                if i == 50 {
                    "ORIGINAL_TOOL_RESULT_MARKER".to_string()
                } else {
                    format!("line {i:03}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let message = ys_core::Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: id.clone(),
                content: original.clone(),
                is_error: true,
            }],
        };
        let degraded = ys_loop::elide_message(&message).expect("60 行结果应触发回合预算降级");
        let degraded_text = match &degraded.content[0] {
            ContentBlock::ToolResult { content, .. } => content,
            _ => unreachable!(),
        };
        assert!(degraded_text.contains(ys_loop::ELIDED_MARKER));
        assert!(!degraded_text.contains("ORIGINAL_TOOL_RESULT_MARKER"));

        let mut session = MemorySession::new();
        session.append(degraded).await.unwrap();
        let mut raw = HashMap::new();
        raw.insert(id.clone(), original);
        let body = transcript_body(&session, &raw, &StopReason::Completed, None, 9, None);
        assert!(body.contains("[tool_result id=call-big is_error=true]"));
        assert!(
            body.contains("ORIGINAL_TOOL_RESULT_MARKER"),
            "必须写事件中的原始工具结果：{body}"
        );
        assert!(!body.contains(ys_loop::ELIDED_MARKER));
        assert!(body.contains("stop_reason=completed"));
        assert!(body.contains("elapsed_ms=9"));
    }

    struct RecordingModel {
        id: String,
        responses: Mutex<VecDeque<ys_model::ModelResponse>>,
        requests: Mutex<Vec<ys_model::ModelRequest>>,
        delay: Duration,
    }

    impl RecordingModel {
        fn new(id: &str) -> Self {
            Self {
                id: id.to_string(),
                responses: Mutex::new(VecDeque::new()),
                requests: Mutex::new(Vec::new()),
                delay: Duration::ZERO,
            }
        }

        fn with_delay(mut self, delay: Duration) -> Self {
            self.delay = delay;
            self
        }

        fn push_tool_call(&self, id: &str, name: &str, arguments: serde_json::Value) -> &Self {
            self.responses
                .lock()
                .unwrap()
                .push_back(ys_model::ModelResponse {
                    message: ys_core::Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::ToolUse {
                            id: ToolCallId(id.to_string()),
                            name: name.to_string(),
                            arguments,
                        }],
                    },
                    usage: Usage::default(),
                    finish_reason: ys_model::FinishReason::ToolCalls,
                });
            self
        }

        fn push_text(&self, text: &str) -> &Self {
            self.responses
                .lock()
                .unwrap()
                .push_back(ys_model::ModelResponse {
                    message: ys_core::Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text: text.to_string(),
                        }],
                    },
                    usage: Usage::default(),
                    finish_reason: ys_model::FinishReason::Stop,
                });
            self
        }

        fn requests(&self) -> Vec<ys_model::ModelRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Model for RecordingModel {
        fn model_id(&self) -> &str {
            &self.id
        }

        async fn complete(
            &self,
            request: ys_model::ModelRequest,
            _sink: &mut dyn ys_model::ModelEventSink,
        ) -> Result<ys_model::ModelResponse, ys_model::ModelError> {
            self.requests.lock().unwrap().push(request);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("RecordingModel 响应队列已耗尽"))
        }
    }

    fn request_has_elided_tool_result(request: &ys_model::ModelRequest) -> bool {
        request.messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::ToolResult { content, .. }
                        if content.contains(ys_loop::ELIDED_MARKER)
                )
            })
        })
    }

    fn unique_subagent_test_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(100);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "yushan_subagent_{label}_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 测试构造：`model` **按值**传入（runner 现在持有 `Arc<dyn Model>`，
    /// 异步派发的后台任务需要 `'static`）⇒ 调用方不再写 `&model`。
    fn test_runner(
        model: impl Model + 'static,
        base: &std::path::Path,
        dir: Arc<SubagentsDir>,
        limits: RunLimits,
    ) -> AppSubagentRunner<'static> {
        test_runner_with_budget(model, base, dir, limits, Duration::from_secs(30))
    }

    /// 同 [`test_runner`]，但 `model` 已包成 `Arc` —— 供「跑完之后还要查 model 录到的请求」
    /// 的用例（`RecordingModel` 含 `Mutex` ⇒ 不可 Clone，只能共享）。
    fn test_runner_arc(
        model: Arc<dyn Model>,
        base: &std::path::Path,
        dir: Arc<SubagentsDir>,
        limits: RunLimits,
    ) -> AppSubagentRunner<'static> {
        use ys_tools_web::FetchedDir;
        AppSubagentRunner::new(
            Some(model),
            None,
            Arc::new(Mutex::new(HashMap::new())),
            "child prompt".to_string(),
            limits,
            Duration::from_secs(30),
            base.to_path_buf(),
            WebConfig::offline(FetchedDir::Temp(base.join("fetched"))),
            dir,
            Arc::new(UsageTotals::default()),
            None,
            Arc::new(PendingResults::default()),
            Arc::new(LiveSubagents::default()),
        )
    }

    fn test_runner_with_budget(
        model: impl Model + 'static,
        base: &std::path::Path,
        dir: Arc<SubagentsDir>,
        limits: RunLimits,
        child_budget: Duration,
    ) -> AppSubagentRunner<'static> {
        test_runner_with_parent(model, base, dir, limits, child_budget, None)
    }

    /// 父边界源**经构造期注入**（而非只经 `run` 的参数）——
    /// 与生产路径一致：app 层把**同一份 `Arc`** 同时给 `make_runner` 与
    /// `AgentPorts`，子 agent 的 `ChildBoundary` 才能透传父的中止状态。
    fn test_runner_with_parent(
        model: impl Model + 'static,
        base: &std::path::Path,
        dir: Arc<SubagentsDir>,
        limits: RunLimits,
        child_budget: Duration,
        parent_boundary: Option<Arc<dyn BoundarySource>>,
    ) -> AppSubagentRunner<'static> {
        test_runner_with_parent_and_results(model, base, dir, limits, child_budget, parent_boundary)
    }

    /// 同 [`test_runner_with_parent`]，但显式给出投递表（异步派发用例需要拿到同一份）。
    fn test_runner_with_parent_and_results(
        model: impl Model + 'static,
        base: &std::path::Path,
        dir: Arc<SubagentsDir>,
        limits: RunLimits,
        child_budget: Duration,
        parent_boundary: Option<Arc<dyn BoundarySource>>,
    ) -> AppSubagentRunner<'static> {
        use ys_tools_web::FetchedDir;

        AppSubagentRunner::new(
            Some(Arc::new(model)),
            None,
            Arc::new(Mutex::new(HashMap::new())),
            "child prompt".to_string(),
            limits,
            child_budget,
            base.to_path_buf(),
            WebConfig::offline(FetchedDir::Temp(base.join("fetched"))),
            dir,
            Arc::new(UsageTotals::default()),
            parent_boundary,
            Arc::new(PendingResults::default()),
            Arc::new(LiveSubagents::default()),
        )
    }

    /// **A5 回归钉（D3）**：父级封顶必须对子 agent 生效。
    ///
    /// 父 `limits.context_window = 200k`、config 的 `context_cap_tokens = 200k`，
    /// 子模型声明 1M ⇒ 子窗口必须是 **200k**，而不是声明的 1M。
    ///
    /// 变异：把 `child_limits_for` 改回 `context_window_for(name)` ⇒ 子窗口变
    /// `1_000_000` ⇒ 本条变红（子 agent 绕过父级封顶）。
    #[test]
    fn child_limits_for_uses_effective_window_not_declared() {
        let base = unique_subagent_test_dir("child_limits_effective");
        let path = base.join("config.json");
        std::fs::write(
            &path,
            r#"{
                "context_window": { "small-sub": 128000 },
                "context_cap_tokens": 200000
            }"#,
        )
        .unwrap();

        let mut config = {
            let _guard = crate::test_env::env_lock();
            crate::config::Config::from_env().unwrap()
        };
        config.model = "parent-model".into();
        config.load_user_config(&path);

        let mut parent_limits = RunLimits::default();
        parent_limits.context_window = 200_000;
        let runner = AppSubagentRunner::new(
            None,
            Some(&config),
            Arc::new(Mutex::new(HashMap::new())),
            "child prompt".to_string(),
            parent_limits,
            Duration::from_secs(30),
            base.clone(),
            WebConfig::offline(ys_tools_web::FetchedDir::Temp(base.join("fetched"))),
            Arc::new(SubagentsDir::at(base.join("subagents"), false)),
            Arc::new(UsageTotals::default()),
            None,
            Arc::new(PendingResults::default()),
            Arc::new(LiveSubagents::default()),
        );

        assert_eq!(
            runner.child_limits_for(Some("MiniMax-M3")).context_window,
            200_000,
            "子模型声明 1M 也必须被父级 cap 封到 200k"
        );
        assert_eq!(
            runner.child_limits_for(None).context_window,
            200_000,
            "不指定模型时继承父窗口"
        );
        assert_eq!(
            runner.child_limits_for(Some("small-sub")).context_window,
            128_000,
            "低于 cap 的子模型保持自身声明值"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// **异步派发的核心**：`spawn` 立刻返回句柄，完成后结果作为 `Boundary::Deferred`
    /// **自动推进父边界源**（不需要 poll）。
    ///
    /// 承重三点：
    ///  1. `spawn` **不阻塞**（子模型慢 200ms，但 `spawn` 立刻返回）；
    ///  2. 结果**自动**出现在边界源里，且带句柄；
    ///  3. 正文含子 agent 的摘要（不是空消息）。
    ///
    /// 变异：① 删掉 `sink.push(...)` ⇒ 第 2 点变红；
    ///       ② 把 `spawn` 改成 `run().await` ⇒ 第 1 点变红。
    #[tokio::test]
    async fn spawn_pushes_deferred_result_without_polling() {
        use std::sync::Arc as StdArc;

        use ys_model::MockModel;
        use ys_protocol::QueueBoundarySource;

        let base = unique_subagent_test_dir("spawn_deferred");
        std::fs::create_dir_all(&base).unwrap();

        let model = MockModel::new("m");
        model.push_text("ASYNC-SUMMARY-MARKER");
        let sink = StdArc::new(QueueBoundarySource::new());
        // 结果落点每回合绑定（生产路径同形）；守卫活到用例末。
        let results = StdArc::new(PendingResults::default());
        let _binding = results.bind(sink.clone());

        let runner = AppSubagentRunner::new(
            Some(Arc::new(model)),
            None,
            Arc::new(Mutex::new(HashMap::new())),
            "child prompt".to_string(),
            RunLimits::new(3),
            Duration::from_secs(30),
            base.clone(),
            WebConfig::offline(ys_tools_web::FetchedDir::Temp(base.join("fetched"))),
            Arc::new(SubagentsDir::at(base.join("artifacts"), false)),
            Arc::new(UsageTotals::default()),
            Some(sink.clone()),
            results,
            Arc::new(LiveSubagents::default()),
        );

        // ① `spawn` 立刻返回（不等子 agent 跑完）。
        let started = Instant::now();
        let handle = runner.spawn("查一下", None, None).expect("应能启动");
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "spawn 必须立刻返回（实际 {:?}）",
            started.elapsed()
        );
        assert_eq!(handle, "sa1", "首个句柄应为 sa1");

        // ② 等后台任务把结果推进边界源（轮询 `take`，最多 5 秒）。
        let mut got = None;
        for _ in 0..250 {
            if let Some(b) = sink.take() {
                got = Some(b);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let Some(Boundary::Deferred { handle, message }) = got else {
            panic!("完成后必须自动推进一条 Boundary::Deferred（不需要 poll）");
        };
        assert_eq!(handle, "sa1", "Deferred 必须带产出它的句柄");

        // ③ 正文含子 agent 的摘要。
        let text: String = message
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            text.contains("ASYNC-SUMMARY-MARKER"),
            "Deferred 正文应包含子 agent 的摘要，实际：{text}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn app_runner_transcript_contains_full_tool_trace() {
        use ys_model::MockModel;

        let base = unique_subagent_test_dir("full_trace");
        std::fs::write(base.join("note.txt"), "NOTE_FROM_TOOL\n").unwrap();
        let model = MockModel::new("m");
        model.push_tool_call("read", serde_json::json!({"path": "note.txt"}));
        model.push_text("FULL_TRACE_FINAL_TEXT_MARKER");
        let dir = Arc::new(SubagentsDir::at(base.join("artifacts"), false));
        let runner = test_runner(model, &base, dir, RunLimits::new(4));

        let out = runner.run("读文件", None, None).await.unwrap();
        assert_eq!(out.stop_reason, StopReason::Completed);
        assert_eq!(out.diagnostic, None);
        let artifact = out.artifact.expect("应写转录");
        let body = std::fs::read_to_string(artifact).unwrap();
        assert!(body.contains("[tool_use id=mock-read name=read]"), "{body}");
        assert!(body.contains("\"path\": \"note.txt\""), "{body}");
        assert!(
            body.contains("[tool_result id=mock-read is_error=false]"),
            "{body}"
        );
        assert!(body.contains("NOTE_FROM_TOOL"), "{body}");
        assert!(
            body.contains("FULL_TRACE_FINAL_TEXT_MARKER"),
            "Text 块原文必须保留：{body}"
        );
        assert!(body.contains("## Run\nstop_reason=completed\n"));
        assert!(body.contains("elapsed_ms="));
        assert!(body.contains("diagnostic=(none)"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn app_runner_turn_budget_elides_session_but_transcript_keeps_raw_result() {
        let base = unique_subagent_test_dir("turn_budget_raw");
        let big = (0..60)
            .map(|i| {
                if i == 50 {
                    "ORIGINAL_TURN_BUDGET_MARKER".to_string()
                } else {
                    format!("line {i:03}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(base.join("big.txt"), big).unwrap();
        std::fs::write(base.join("small.txt"), "SECOND_TOOL_RESULT\n").unwrap();

        let model = Arc::new(RecordingModel::new("recording"));
        model
            .push_tool_call("call-big", "read", serde_json::json!({"path": "big.txt"}))
            .push_tool_call(
                "call-small",
                "read",
                serde_json::json!({"path": "small.txt"}),
            )
            .push_text("TURN_BUDGET_FINAL_TEXT_MARKER");
        let mut limits = RunLimits::new(5);
        limits.max_turn_tool_result_tokens = 1;
        let dir = Arc::new(SubagentsDir::at(base.join("artifacts"), false));
        let runner = test_runner_arc(model.clone(), &base, dir, limits);

        let out = runner.run("预算回归", None, None).await.unwrap();
        assert_eq!(out.stop_reason, StopReason::Completed);
        let requests = model.requests();
        assert_eq!(requests.len(), 3, "两批工具后应发生第三次模型请求");
        assert!(
            request_has_elided_tool_result(&requests[2]),
            "第二次工具结果落盘后，下一次模型请求必须看到被 turn budget 降级的结果"
        );

        let artifact = out.artifact.expect("应写转录");
        let body = std::fs::read_to_string(artifact).unwrap();
        assert!(
            body.contains("ORIGINAL_TURN_BUDGET_MARKER"),
            "落盘转录必须保留事件里的原始 ToolResult：{body}"
        );
        assert!(
            !body.contains(ys_loop::ELIDED_MARKER),
            "落盘转录不得使用已被预算改写的会话正文：{body}"
        );
        assert!(body.contains("TURN_BUDGET_FINAL_TEXT_MARKER"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn app_runner_later_tool_result_with_same_id_wins_in_transcript() {
        let base = unique_subagent_test_dir("duplicate_id");
        std::fs::write(base.join("first.txt"), "FIRST_DUP_RESULT\n").unwrap();
        std::fs::write(base.join("second.txt"), "SECOND_DUP_RESULT\n").unwrap();
        let model = RecordingModel::new("recording");
        model
            .push_tool_call("call-dup", "read", serde_json::json!({"path": "first.txt"}))
            .push_tool_call(
                "call-dup",
                "read",
                serde_json::json!({"path": "second.txt"}),
            )
            .push_text("DUPLICATE_ID_FINAL_TEXT");
        let dir = Arc::new(SubagentsDir::at(base.join("artifacts"), false));
        let runner = test_runner(model, &base, dir, RunLimits::new(5));

        let out = runner.run("重复 id", None, None).await.unwrap();
        let body = std::fs::read_to_string(out.artifact.expect("应写转录")).unwrap();
        assert!(
            !body.contains("FIRST_DUP_RESULT"),
            "同 id 的较早 ToolResult 应被后到值覆盖：{body}"
        );
        assert_eq!(
            body.matches("SECOND_DUP_RESULT").count(),
            2,
            "两个同 id 会话块都应使用后到的原始结果：{body}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// provider 错误（`Run` 返回 `Err`）⇒ `Failed` + **无**取消归因。
    ///
    /// 改名自 `app_runner_run_error_is_unknown_cause_and_sanitized`：旧名与旧断言钉的是
    /// 「错误归成 `Cancelled` + `cause=unknown`」—— 那正是本次修掉的 bug
    /// （诊断行自相矛盾，且把真故障埋在「用户取消」里）。
    ///
    /// 保留的部分：错误文本的清洗（去换行、去控制字符）与诊断长度上限。
    #[tokio::test]
    async fn app_runner_run_error_is_failed_without_cause_and_sanitized() {
        use ys_model::MockModel;

        let base = unique_subagent_test_dir("run_error");
        let model = MockModel::new("m");
        // 排两次：产品路径会重试 1 次（`AppSubagentRunner::new` 无条件开启），
        // 只排一次会让重试时队列空而 panic。本用例验证的是「额度耗尽后如实上报」。
        model.push_error("provider failed\nsecond line\r\u{7}");
        model.push_error("provider failed\nsecond line\r\u{7}");
        let dir = Arc::new(SubagentsDir::at(base.join("artifacts"), false));
        let runner = test_runner(model, &base, dir, RunLimits::new(2));

        let out = runner.run("失败", None, None).await.unwrap();
        assert_eq!(
            out.stop_reason,
            StopReason::Failed,
            "provider 错误必须是故障"
        );
        let diagnostic = out.diagnostic.expect("Run Err 必须有诊断");
        assert!(diagnostic.starts_with(DIAGNOSTIC_PREFIX));
        assert!(diagnostic.contains("stop_reason=failed"));
        assert!(
            !diagnostic.contains("cause="),
            "故障不是取消 ⇒ 不得有取消归因: {diagnostic}"
        );
        assert!(diagnostic.contains("elapsed_ms="));
        assert!(diagnostic.contains("provider failed second line"));
        assert!(!diagnostic.chars().any(char::is_control));
        assert!(diagnostic.len() <= MAX_DIAGNOSTIC_BYTES);

        let artifact = out.artifact.expect("即使 Run Err 也应尽量写转录");
        let body = std::fs::read_to_string(artifact).unwrap();
        assert!(
            !body.contains("cause=unknown"),
            "转录不得带取消归因: {body}"
        );
        assert!(body.contains("diagnostic=[diagnostic: stop_reason=failed"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn app_runner_write_failure_is_reported_in_diagnostic() {
        use ys_model::MockModel;

        let base = unique_subagent_test_dir("write_failure");
        let blocker = base.join("not-a-directory");
        std::fs::write(&blocker, "file blocks create_dir_all").unwrap();
        let model = MockModel::new("m");
        model.push_text("WRITE_FAILURE_SUMMARY_MARKER");
        let runner = test_runner(
            model,
            &base,
            Arc::new(SubagentsDir::at(blocker, false)),
            RunLimits::new(2),
        );

        let out = runner.run("写失败", None, None).await.unwrap();
        assert_eq!(out.stop_reason, StopReason::Completed);
        assert_eq!(out.artifact, None);
        assert!(
            out.summary.contains("WRITE_FAILURE_SUMMARY_MARKER"),
            "写失败不得吞掉已有摘要：{}",
            out.summary
        );
        assert!(out.summary.contains("## Files"));
        let diagnostic = out.diagnostic.expect("写失败必须产生诊断");
        assert!(diagnostic.starts_with(DIAGNOSTIC_PREFIX));
        assert!(diagnostic.contains("stop_reason=completed"));
        assert!(diagnostic.contains("transcript write failed"));
        assert!(!diagnostic.contains("cause="));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn app_runner_parent_cancellation_wins_over_child_budget() {
        use ys_model::MockModel;

        let base = unique_subagent_test_dir("parent_cancel");
        let parent = Arc::new(QueueBoundarySource::new());
        parent.push(Boundary::Abort);
        let model = MockModel::new("must-not-run");
        let dir = Arc::new(SubagentsDir::at(base.join("artifacts"), false));
        // 父边界源经**构造期**注入 —— 与生产路径一致（app 层把同一份 `Arc`
        // 给 `make_runner` 与 `AgentPorts`），子 `ChildBoundary` 才透传得到中止。
        let runner = test_runner_with_parent(
            model,
            &base,
            dir,
            RunLimits::new(2),
            Duration::from_secs(30),
            Some(parent.clone()),
        );

        let out = runner
            .run("取消", None, Some(parent.as_ref()))
            .await
            .unwrap();
        assert_eq!(out.stop_reason, StopReason::Cancelled);
        let diagnostic = out.diagnostic.expect("取消必须有诊断");
        assert!(diagnostic.contains("cause=parent-turn-cancelled"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn app_runner_watchdog_child_budget_is_attributed() {
        let base = unique_subagent_test_dir("child_budget");
        std::fs::write(base.join("note.txt"), "note\n").unwrap();
        let model = RecordingModel::new("slow-child").with_delay(Duration::from_millis(40));
        model.push_tool_call(
            "budget-read",
            "read",
            serde_json::json!({"path": "note.txt"}),
        );
        let dir = Arc::new(SubagentsDir::at(base.join("artifacts"), false));
        let runner = test_runner_with_budget(
            model,
            &base,
            dir,
            RunLimits::new(2),
            Duration::from_millis(5),
        );

        let out = runner.run("子预算", None, None).await.unwrap();
        assert_eq!(out.stop_reason, StopReason::Cancelled);
        let diagnostic = out.diagnostic.expect("子预算取消必须有诊断");
        assert!(
            diagnostic.contains("cause=child-budget"),
            "真实 watchdog 应归因为子预算：{diagnostic}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn app_runner_parent_cancel_wins_when_watchdog_budget_also_expires() {
        let base = unique_subagent_test_dir("parent_and_budget");
        std::fs::write(base.join("note.txt"), "note\n").unwrap();
        let model = RecordingModel::new("slow-child").with_delay(Duration::from_millis(80));
        model.push_tool_call(
            "budget-read",
            "read",
            serde_json::json!({"path": "note.txt"}),
        );
        let parent = Arc::new(QueueBoundarySource::new());
        let dir = Arc::new(SubagentsDir::at(base.join("artifacts"), false));
        let runner = test_runner_with_parent(
            model,
            &base,
            dir,
            RunLimits::new(2),
            Duration::from_millis(5),
            Some(parent.clone()),
        );

        let cancel_parent = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            parent.push(Boundary::Abort);
        };
        let (out, ()) = tokio::join!(
            runner.run("父取消", None, Some(parent.as_ref())),
            cancel_parent
        );
        let out = out.unwrap();
        assert_eq!(out.stop_reason, StopReason::Cancelled);
        let diagnostic = out.diagnostic.expect("取消必须有诊断");
        assert!(
            diagnostic.contains("cause=parent-turn-cancelled"),
            "父取消与子预算都到点时父取消优先：{diagnostic}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
