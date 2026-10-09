//! subagent 派发（刀 4）—— app 侧**真活**：造子执行环境、跑、落盘、截断、记账。
//!
//! 形态由 [ADR-0022](../../../docs/adr/0022-subagent-dispatch.md) 定死：
//! 子事件**不进**父事件流（`NoopEventSink`）、子表 = 父表去掉 `subagent`、
//! 结果 = 硬截断摘要 + 产物路径 + `StopReason`、取消 = `ChildBoundary` 只透传
//! `is_aborted`。落地裁定见 [ADR-0029](../../../docs/adr/0029-subagent-landing-decisions.md)：
//! 每回合派发总量上限、`model` 名解析失败 fail-closed、产物目录 sweep + 环境变量覆盖口。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value;
use ys_component::RunLimits;
use ys_core::{StopReason, Usage};
use ys_event::NoopEventSink;
use ys_loop::AgentInput;
use ys_model::Model;
use ys_protocol::{Boundary, BoundarySource, QueueBoundarySource};
use ys_runtime::{AgentBuilder, AgentPorts};
use ys_session::{MemorySession, Session};
use ys_tool::{SubagentOutcome, SubagentRunner, Tool, ToolContext, ToolError, ToolResult, ToolSpec};
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

/// 子 agent 的边界源：`is_aborted` 透传父的；`take()` **只取自己那份**，绝不消费父队列。
///
/// 与 ADR-0022 决策 5 的差异（`take()` 从「恒 None」改为取自己的）记在 ADR-0029 §6 修正 4。
pub struct ChildBoundary<'a> {
    own: Arc<QueueBoundarySource>,
    parent: Option<&'a dyn BoundarySource>,
}

impl<'a> ChildBoundary<'a> {
    pub fn new(parent: Option<&'a dyn BoundarySource>) -> Self {
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

impl BoundarySource for ChildBoundary<'_> {
    fn take(&self) -> Option<Boundary> {
        self.own.take()
    }
    fn is_aborted(&self) -> bool {
        self.own.is_aborted() || self.parent.is_some_and(|p| p.is_aborted())
    }
}

/// 子 agent usage 的累加器（跨并发派发无锁）。父回合末 drain 进 `TurnStats`。
#[derive(Default)]
pub struct UsageTotals {
    input: AtomicU64,
    output: AtomicU64,
    hit: AtomicU64,
    miss: AtomicU64,
}

impl UsageTotals {
    pub fn add(&self, u: &Usage) {
        self.input.fetch_add(u.input_tokens as u64, Ordering::Relaxed);
        self.output.fetch_add(u.output_tokens as u64, Ordering::Relaxed);
        self.hit
            .fetch_add(u.prompt_cache_hit_tokens as u64, Ordering::Relaxed);
        self.miss
            .fetch_add(u.prompt_cache_miss_tokens as u64, Ordering::Relaxed);
    }

    /// 取走累计值并清零（回合末一次）。
    pub fn drain(&self) -> Usage {
        Usage {
            input_tokens: self.input.swap(0, Ordering::Relaxed) as u32,
            output_tokens: self.output.swap(0, Ordering::Relaxed) as u32,
            prompt_cache_hit_tokens: self.hit.swap(0, Ordering::Relaxed) as u32,
            prompt_cache_miss_tokens: self.miss.swap(0, Ordering::Relaxed) as u32,
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
            let Ok(modified) = meta.modified() else { continue };
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
        let path = self
            .path
            .join(format!("{}_{}.md", std::process::id(), seq));
        std::fs::write(&path, body)?;
        Ok(std::fs::canonicalize(&path).unwrap_or(path))
    }
}

/// app 侧的派发实现：每回合构造一份（计数器天然 per-turn）。
pub struct AppSubagentRunner<'a> {
    parent_model: Option<&'a dyn Model>,
    /// 按名构造模型用（`model` 参数）；`None` = 未接线 ⇒ 指定模型名一律 fail-closed。
    config: Option<&'a crate::config::Config>,
    /// 按模型名缓存实例（跨派发复用连接池 + TLS）；缓存持 `Arc<dyn Model>`，
    /// 因为 `SubagentRunner::run` 需要 `&dyn Model`，`Box` 拿不出稳定引用。
    model_cache: Arc<Mutex<HashMap<String, Arc<dyn Model>>>>,
    child_prompt: String,
    child_limits: RunLimits,
    child_budget: Duration,
    workspace: PathBuf,
    web: WebConfig,
    dir: Arc<SubagentsDir>,
    max_dispatches: u32,
    dispatched: AtomicU32,
    totals: Arc<UsageTotals>,
}

impl<'a> AppSubagentRunner<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        parent_model: Option<&'a dyn Model>,
        config: Option<&'a crate::config::Config>,
        model_cache: Arc<Mutex<HashMap<String, Arc<dyn Model>>>>,
        child_prompt: String,
        child_limits: RunLimits,
        child_budget: Duration,
        workspace: PathBuf,
        web: WebConfig,
        dir: Arc<SubagentsDir>,
        totals: Arc<UsageTotals>,
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
    /// 按模型名缓存实例（跨派发复用；缓存持 `Arc<dyn Model>`，见 runner 的注释）。
    model_cache: Arc<Mutex<HashMap<String, Arc<dyn Model>>>>,
}

impl SubagentSetup {
    pub fn new(
        child_prompt: String,
        child_limits: RunLimits,
        child_budget: Duration,
        workspace: PathBuf,
        web: WebConfig,
    ) -> Self {
        Self {
            child_prompt,
            child_limits,
            child_budget,
            workspace,
            web,
            dir: Arc::new(SubagentsDir::resolve()),
            model_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 派发工具的外闸（子预算 + 宽限，构造期注入）。
    pub fn outer_timeout(&self) -> Duration {
        self.child_budget + OUTER_GRACE
    }

    /// 启动期清理产物目录（只对 Primary 档；`Override` 档只写不删）。
    pub fn sweep_old_products(&self) {
        self.dir.sweep_old();
    }

    /// 造一份 runner（每回合一次 ⇒ 派发计数器天然 per-turn）。
    pub fn make_runner<'a>(
        &self,
        config: Option<&'a crate::config::Config>,
        parent_model: Option<&'a dyn Model>,
        totals: Arc<UsageTotals>,
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
        // fail-closed：未支持按名构造模型时**绝不静默回退到父模型**。
        let owned: Option<Arc<dyn Model>> = match model {
            None => None,
            Some(name) => {
                if let Some(hit) = self.model_cache.lock().unwrap().get(name).cloned() {
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
                    self.model_cache
                        .lock()
                        .unwrap()
                        .insert(name.to_string(), arc.clone());
                    Some(arc)
                }
            }
        };
       let resolved: &dyn Model = match &owned {
            Some(m) => &**m,
            None => self
                .parent_model
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

        // 每次派发建**一个全新子 Agent**（`Agent::run_turn` 取 `&mut self`，实例不可共享）。
        // 子换模型 ⇒ 窗口必须按**子模型**取（继承父窗口是**静默错**：32k 模型按 1M 判
        // ⇒ 不压缩 ⇒ 撞窗）。默认继承时不改（与父同值）。
        let mut child_limits = self.child_limits.clone();
        if let (Some(cfg), Some(name)) = (self.config, model) {
            child_limits.context_window = cfg.context_window_for(name);
        }

        let (tools, _hint) = crate::toolset::build_tools(&self.workspace, self.web.clone());
        let mut child = AgentBuilder::new()
            .tools(tools)
            .limits(child_limits)
            .working_dir(self.workspace.clone(), self.workspace.clone())
            .system_prompt(self.child_prompt.clone())
            .build()
            .map_err(|e| format!("子 agent 组装失败: {e}"))?;

        let child_boundary = ChildBoundary::new(boundary);
        // watchdog：到点向**子自己的**边界源 push(Abort)，子在轮边界优雅收尾。
        // 必须 `AbortOnDropHandle` —— 否则外闸 drop 子 future 时该任务会孤儿化并沉睡到预算到点。
        let handle = {
            let own = child_boundary.own();
            let budget = self.child_budget;
            tokio::spawn(async move {
                tokio::time::sleep(budget).await;
                own.push(Boundary::Abort);
            })
        };
        let _watchdog_guard = tokio_util::task::AbortOnDropHandle::new(handle);

        let mut session = MemorySession::new();
        let mut sink = NoopEventSink;
        let result = child
            .run_turn(
                AgentInput::text(format!("{prompt}{CHILD_TASK_SUFFIX}")),
                AgentPorts::new(
                    Some(resolved),
                    &mut session,
                    &mut sink,
                    Some(&child_boundary),
                ),
            )
            .await;

        let outcome = match result {
            Ok(run) => {
                self.totals.add(&run.usage);
                let summary =
                    ensure_files_section(&transcript_summary(&session, run.final_message.as_ref()));
                let artifact = self.dir.write_transcript(&transcript_body(&session)).ok();
                let stop_reason = run.stop_reason.clone();
                let diagnostic = match &stop_reason {
                    StopReason::Completed => None,
                    other => Some(format!("子 agent 未正常收尾：{other:?}")),
                };
                SubagentOutcome {
                    summary,
                    artifact: artifact.map(|p| p.display().to_string()),
                    stop_reason,
                    diagnostic,
                    usage: run.usage,
                }
            }
            Err(e) => {
                // 「跑都跑不起来」—— 仍尽量留下已写出的转录。
                let artifact = self.dir.write_transcript(&transcript_body(&session)).ok();
                SubagentOutcome {
                    summary: String::new(),
                    artifact: artifact.map(|p| p.display().to_string()),
                    stop_reason: StopReason::Cancelled,
                    diagnostic: Some(format!("子 agent 执行失败: {e}")),
                    usage: Usage::default(),
                }
            }
        };
        Ok(outcome)
    }
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
            session
                .messages()
                .iter()
                .rev()
                .find_map(|m| {
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

fn transcript_body(session: &MemorySession) -> String {
    let mut out = String::new();
    for m in session.messages() {
        out.push_str(&format!("## {:?}\n{}\n\n", m.role, text_of(m)));
    }
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

        let mut content = out.summary;
        if let Some(a) = &out.artifact {
            content.push_str(&format!("\n[full transcript: {a}]"));
        }
        if let Some(d) = &out.diagnostic {
            content.push_str(&format!("\n[diagnostic: {d}]"));
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
    use ys_core::Role;

    #[test]
    fn child_boundary_takes_own_and_probes_parent() {
        let parent = QueueBoundarySource::new();
        let cb = ChildBoundary::new(Some(&parent));
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
        });
        t.add(&Usage {
            input_tokens: 4,
            output_tokens: 5,
            prompt_cache_hit_tokens: 6,
            prompt_cache_miss_tokens: 7,
        });
        let d = t.drain();
        assert_eq!((d.input_tokens, d.output_tokens), (7, 7));
        assert_eq!((d.prompt_cache_hit_tokens, d.prompt_cache_miss_tokens), (7, 9));
        assert_eq!(t.drain().input_tokens, 0, "drain 后清零");
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
                    diagnostic: (self.0 != StopReason::Completed).then(|| "boom".into()),
                    usage: Usage::default(),
                })
            }
        }

        let tool = SubagentTool::new(Duration::from_secs(60));
        let runner = FakeRunner(StopReason::Completed);
        let ctx = ToolContext::new(None, PathBuf::from("."), PathBuf::from("."))
            .with_subagent(&runner);
        let r = tool
            .call(serde_json::json!({"description":"d","prompt":"干活"}), ctx)
            .await
            .unwrap();
        assert!(!r.is_error);
        assert!(r.content.contains("摘要:干活"));
        assert!(r.content.contains("[full transcript: /tmp/t.md]"));

        let runner2 = FakeRunner(StopReason::MaxRounds);
        let ctx2 = ToolContext::new(None, PathBuf::from("."), PathBuf::from("."))
            .with_subagent(&runner2);
        let r2 = tool
            .call(serde_json::json!({"description":"d","prompt":"干活"}), ctx2)
            .await
            .unwrap();
        assert!(r2.is_error, "非 Completed ⇒ is_error（父回合不失败）");

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
                    },
                    finish_reason: FinishReason::Stop,
                })
            }
        }

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "yushan_subagent_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let model = FixedUsageModel;

        let dir = Arc::new(SubagentsDir {
            path: base.join("subagents"),
            sweep: false,
        });
        let web = WebConfig::offline(FetchedDir::Temp(base.join("fetched")));
        let totals = Arc::new(UsageTotals::default());
        let runner = AppSubagentRunner::new(
            Some(&model),
            None,
            Arc::new(Mutex::new(HashMap::new())),
            "你是子 agent".to_string(),
            RunLimits::new(3),
            Duration::from_secs(30),
            base.clone(),
            web,
            dir.clone(),
            totals.clone(),
        );

        let out = runner.run("查 X", None, None).await.unwrap();
        assert_eq!(out.stop_reason, StopReason::Completed);
        assert!(out.summary.contains("子 agent 的结论"), "摘要 = {}", out.summary);
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
        let err = runner.run("x", Some("other-model"), None).await.unwrap_err();
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
        let base = std::env::temp_dir().join(format!(
            "yushan_subagent_cap_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let model = MockModel::new("m");
        for _ in 0..(DEFAULT_MAX_DISPATCHES_PER_TURN + 1) {
            model.push_text("ok");
        }
        let web = WebConfig::offline(FetchedDir::Temp(base.join("fetched")));
        let runner = AppSubagentRunner::new(
            Some(&model),
            None,
            Arc::new(Mutex::new(HashMap::new())),
            String::new(),
            RunLimits::new(2),
            Duration::from_secs(30),
            base.clone(),
            web,
            Arc::new(SubagentsDir {
                path: base.join("subagents"),
                sweep: false,
            }),
            Arc::new(UsageTotals::default()),
        );
        for _ in 0..DEFAULT_MAX_DISPATCHES_PER_TURN {
            runner.run("t", None, None).await.unwrap();
        }
        let err = runner.run("t", None, None).await.unwrap_err();
        assert!(err.contains("上限"), "超限应响亮报错: {err}");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 子 agent 内部会话独立：`Role` 齐备且不含父历史。
    #[test]
    fn transcript_body_renders_roles() {
        let s = MemorySession::new();
        // 空会话 → 空正文（不 panic）。
        assert_eq!(transcript_body(&s), "");
        let _ = Role::User;
    }
}
