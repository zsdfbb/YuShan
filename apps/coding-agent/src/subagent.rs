//! subagent 派发（刀 4）—— app 侧**真活**：造子执行环境、跑、落盘、截断、记账。
//!
//! 形态由 [ADR-0022](../../../docs/adr/0022-subagent-dispatch.md) 定死：
//! 子事件只在本地 `CollectingSink` 收集、**不进**父事件流；子表 = 父表去掉 `subagent`、
//! 结果 = 硬截断摘要 + 产物路径 + `StopReason`、取消 = `ChildBoundary` 只透传
//! `is_aborted`。落地裁定见 [ADR-0029](../../../docs/adr/0029-subagent-landing-decisions.md)：
//! 每回合派发总量上限、`model` 名解析失败 fail-closed、产物目录 sweep + 环境变量覆盖口。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use ys_component::RunLimits;
use ys_core::{ContentBlock, StopReason, ToolCallId, Usage};
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
///
/// 与 ADR-0022 决策 5 的差异（`take()` 从「恒 None」改为取自己的）记在 ADR-0029 §6 修正 4。
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
}

/// 子 agent usage 的累加器（跨并发派发无锁）。父回合末 drain 进 `TurnStats`。
#[derive(Default)]
pub struct UsageTotals {
    input: AtomicU64,
    output: AtomicU64,
    hit: AtomicU64,
    miss: AtomicU64,
    /// 推理 token 累计；仅当至少一次子调用上报了 `Some(_)` 时才有意义
    /// （是否上报由 `reasoning_reported` 记录）。
    reasoning: AtomicU64,
    /// 是否有任一子调用上报了 `reasoning_tokens`（`Some(0)` 也算上报）。
    reasoning_reported: AtomicBool,
}

impl UsageTotals {
    pub fn add(&self, u: &Usage) {
        self.input
            .fetch_add(u.input_tokens as u64, Ordering::Relaxed);
        self.output
            .fetch_add(u.output_tokens as u64, Ordering::Relaxed);
        self.hit
            .fetch_add(u.prompt_cache_hit_tokens as u64, Ordering::Relaxed);
        self.miss
            .fetch_add(u.prompt_cache_miss_tokens as u64, Ordering::Relaxed);
        // 只有上报了才累加并置位：`None` = 未上报，`Some(0)` = 真 0，两者不可折叠。
        if let Some(v) = u.reasoning_tokens {
            self.reasoning.fetch_add(v as u64, Ordering::Relaxed);
            self.reasoning_reported.store(true, Ordering::Relaxed);
        }
    }

    /// 取走累计值并清零（回合末一次）。
    ///
    /// `reasoning_tokens`：任何子调用报过 ⇒ `Some(累计)`（含累计为 0 的情形）；
    /// 一次都没报过 ⇒ `None`。不能像其他字段那样恒写 0 —— 那会把「未上报」与「真 0」
    /// 折叠，且会把已上报的推理 token 静默丢弃。
    pub fn drain(&self) -> Usage {
        let reported = self.reasoning_reported.swap(false, Ordering::Relaxed);
        let reasoning = self.reasoning.swap(0, Ordering::Relaxed) as u32;
        Usage {
            input_tokens: self.input.swap(0, Ordering::Relaxed) as u32,
            output_tokens: self.output.swap(0, Ordering::Relaxed) as u32,
            prompt_cache_hit_tokens: self.hit.swap(0, Ordering::Relaxed) as u32,
            prompt_cache_miss_tokens: self.miss.swap(0, Ordering::Relaxed) as u32,
            reasoning_tokens: reported.then_some(reasoning),
        }
    }
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
    ) -> Self {
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
}

impl SubagentSetup {
    /// 产物目录**由装配方显式给出**（不在构造期隐式读 `YUSHAN_SUBAGENTS_DIR` / `HOME`）——
    /// 落点决策与 `WebConfig::from_env` / `sweep_fetched` 同在装配点完成，且测试可注入
    /// 临时目录，不会写进用户的 `~/.yushan/subagents/`。
    ///
    /// `dir` 以 `Arc` 传入、**跨重建共享**（供 [`crate::assembly::AgentFactory`] 用）：
    /// 工作区切换时同一进程内会多次重建本结构，共享同一份产物目录描述（`Arc` 只复制
    /// 指针），落点与 sweep 档位因此跨重建一致。
    pub fn shared(
        child_prompt: String,
        child_limits: RunLimits,
        child_budget: Duration,
        workspace: PathBuf,
        web: WebConfig,
        dir: Arc<SubagentsDir>,
    ) -> Self {
        Self {
            child_prompt,
            child_limits,
            child_budget,
            workspace,
            web,
            dir,
            model_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 派发工具的外闸（子预算 + 宽限，构造期注入）。
    pub fn outer_timeout(&self) -> Duration {
        self.child_budget + OUTER_GRACE
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
        )
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
        let resolved: Arc<dyn Model> = match owned {
            Some(m) => m,
            None => self
                .parent_model
                .clone()
                .ok_or_else(|| "无可用模型（未登录 / 未配置）".to_string())?,
        };

        // 每回合派发总量上限（ADR-0022 决策 15）。
        let n = self.dispatched.fetch_add(1, Ordering::Relaxed);
        if n >= self.max_dispatches {
            return Err(format!(
                "本回合派发次数已达上限（{}），拒绝继续派发（防成本爆炸）",
                self.max_dispatches
            ));
        }

        // 子换 model ⇒ 窗口必须按**子模型**取（继承父窗口是**静默错**：32k 模型按 1M 判
        // ⇒ 不压缩 ⇒ 撞窗）。默认继承时不改（与父同值）。
        let mut child_limits = self.child_limits.clone();
        if let (Some(cfg), Some(name)) = (self.config, model) {
            child_limits.context_window = cfg.context_window_for(name);
        }

        // 执行交给 owned 的 `ChildTask` —— 同步 `run` 与异步 `spawn` **共用同一份**
        // 执行逻辑（否则两条路径的行为会漂移：结果打包、诊断、usage 上卷全在手写两份）。
        Ok(self
            .child_task(resolved, child_limits)
            .execute(prompt.to_string())
            .await)
    }
}

impl AppSubagentRunner<'_> {
    /// 把「一次派发」需要的**全部输入**收成 [`ChildTask`]。
    ///
    /// `model` / `child_limits` 由调用方**在回合内**解析好（解析要读 `config`，
    /// 而 `config` 是借用、不可 `'static`）⇒ 收成的 `ChildTask` 只含 owned 字段。
    fn child_task(&self, model: Arc<dyn Model>, child_limits: RunLimits) -> ChildTask {
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
                // 建不起来：留一份最小转录 + 诊断，语义与「跑起来但失败」一致。
                let stop_reason = StopReason::Cancelled;
                let cause = Some(CancelCause::Unknown);
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
                self.totals.add(&run.usage);
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
                // 「跑都跑不起来」仍尽量留转录；错误文本只进诊断，不伪装成父/子取消。
                let stop_reason = StopReason::Cancelled;
                let cause = Some(CancelCause::Unknown);
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
        let out = runner
            .run(prompt, model, ctx.boundary)
            .await
            .map_err(ToolError::Execution)?;

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

    #[test]
    fn usage_totals_accumulate_and_drain() {
        let t = UsageTotals::default();
        t.add(&Usage {
            input_tokens: 3,
            output_tokens: 2,
            prompt_cache_hit_tokens: 1,
            prompt_cache_miss_tokens: 2,
            reasoning_tokens: None,
        });
        t.add(&Usage {
            input_tokens: 4,
            output_tokens: 5,
            prompt_cache_hit_tokens: 6,
            prompt_cache_miss_tokens: 7,
            reasoning_tokens: None,
        });
        let d = t.drain();
        assert_eq!((d.input_tokens, d.output_tokens), (7, 7));
        assert_eq!(
            (d.prompt_cache_hit_tokens, d.prompt_cache_miss_tokens),
            (7, 9)
        );
        assert_eq!(t.drain().input_tokens, 0, "drain 后清零");
    }

    /// 子调用全都不报 reasoning ⇒ 上卷为 `None`（未上报，不得伪造 0）。
    ///
    /// 变异说明：把 `drain` 的 `reasoning_tokens` 改成恒 `Some(reasoning)` ⇒ 本测试变红。
    #[test]
    fn usage_totals_reasoning_none_when_unreported() {
        let t = UsageTotals::default();
        t.add(&Usage {
            input_tokens: 1,
            ..Default::default()
        });
        t.add(&Usage {
            input_tokens: 2,
            ..Default::default()
        });
        assert_eq!(t.drain().reasoning_tokens, None);
    }

    /// 有一个子调用报 `Some(0)` ⇒ 上卷为 `Some(0)`（真 0 不能折叠成 `None`）。
    ///
    /// 变异说明：`add` 里按 `v > 0` 而非 `Some(_)` 置位 ⇒ 本测试变红；
    /// `drain` 里写成 `(reasoning > 0).then_some(...)` ⇒ 本测试同样变红。
    #[test]
    fn usage_totals_reasoning_keeps_reported_zero() {
        let t = UsageTotals::default();
        t.add(&Usage {
            reasoning_tokens: Some(0),
            ..Default::default()
        });
        assert_eq!(t.drain().reasoning_tokens, Some(0));
    }

    /// 多次上报 ⇒ 相加；`None` 不影响已上报的累计。
    ///
    /// 变异说明：把 `add` 的 `reasoning` 累加删掉 / 恒置 0 ⇒ 断言变红；
    /// 把 `drain` 硬写 `None`（修复前的行为）⇒ 断言同样变红。
    #[test]
    fn usage_totals_reasoning_sums_reported_values() {
        let t = UsageTotals::default();
        t.add(&Usage {
            reasoning_tokens: Some(3),
            ..Default::default()
        });
        t.add(&Usage {
            reasoning_tokens: None,
            ..Default::default()
        });
        t.add(&Usage {
            reasoning_tokens: Some(4),
            ..Default::default()
        });
        assert_eq!(t.drain().reasoning_tokens, Some(7));
        // drain 后清零，且「是否上报」也复位。
        assert_eq!(t.drain().reasoning_tokens, None);
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
                        prompt_cache_hit_tokens: 9,
                        prompt_cache_miss_tokens: 2,
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
        let up = totals.drain();
        assert_eq!((up.input_tokens, up.output_tokens), (11, 7), "usage 应上卷");
        assert_eq!(
            (up.prompt_cache_hit_tokens, up.prompt_cache_miss_tokens),
            (9, 2),
            "缓存读数也应上卷"
        );

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
        )
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

    #[tokio::test]
    async fn app_runner_run_error_is_unknown_cause_and_sanitized() {
        use ys_model::MockModel;

        let base = unique_subagent_test_dir("run_error");
        let model = MockModel::new("m");
        model.push_error("provider failed\nsecond line\r\u{7}");
        let dir = Arc::new(SubagentsDir::at(base.join("artifacts"), false));
        let runner = test_runner(model, &base, dir, RunLimits::new(2));

        let out = runner.run("失败", None, None).await.unwrap();
        assert_eq!(out.stop_reason, StopReason::Cancelled);
        let diagnostic = out.diagnostic.expect("Run Err 必须有诊断");
        assert!(diagnostic.starts_with(DIAGNOSTIC_PREFIX));
        assert!(diagnostic.contains("stop_reason=cancelled"));
        assert!(diagnostic.contains("cause=unknown"));
        assert!(diagnostic.contains("elapsed_ms="));
        assert!(diagnostic.contains("provider failed second line"));
        assert!(!diagnostic.chars().any(char::is_control));
        assert!(diagnostic.len() <= MAX_DIAGNOSTIC_BYTES);

        let artifact = out.artifact.expect("即使 Run Err 也应尽量写转录");
        let body = std::fs::read_to_string(artifact).unwrap();
        assert!(body.contains("cause=unknown"));
        assert!(body.contains("diagnostic=[diagnostic: stop_reason=cancelled"));

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
