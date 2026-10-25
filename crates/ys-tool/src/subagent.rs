//! 一次性「派发 subagent」能力 —— 把一段**自包含任务文本**交给一个隔离的子 agent，拿回摘要。
//!
//! 形状刻意窄，与 [`crate::Summarizer`] 同层同形：`ys-model` 依赖 `ys-tool`
//! （为 `ToolSpec`）⇒ 反向依赖成环 ⇒ 此处**不得**出现 `Model` / `ModelRequest` /
//! `Session` / `AgentEvent`。允许 `ys-core` 的 `StopReason` / `Usage`（`ys-tool`
//! 已依赖 `ys-core`），也允许 `ys-protocol` 的 `BoundarySource`（`ToolContext` 已在用）。

use async_trait::async_trait;
use std::future::Future;
use std::pin::Pin;
use ys_core::{StopReason, Usage};
use ys_protocol::BoundarySource;

/// 一次派发的结果。
///
/// **摘要已由实现方按预算硬截断** —— 截断责任在代码，不在提示词。
#[derive(Debug, Clone)]
pub struct SubagentOutcome {
    /// 给模型看的摘要（已截断到实现方选定的上限）。
    pub summary: String,
    /// 完整转录的**绝对路径**（落盘）；失败时若已写出部分内容也尽量给。
    pub artifact: Option<String>,
    /// 子 agent 的终止原因。**复用 `ys_core::StopReason`，不新造词汇**。
    ///
    /// 非 `Completed` ⇒ 父侧 `ToolResult{is_error: true}`（ADR-0001 错误结果通道），
    /// 父回合**不因此失败**。
    pub stop_reason: StopReason,
    /// 非 `Completed` 时的**安全失败详情**（provider 报错原文等），实现方须硬上限截断。
    pub diagnostic: Option<String>,
    /// 子 agent 累计的 token 用量 —— 供父**上卷进 `TurnStats`**。
    pub usage: Usage,
}

/// 一次性派发能力：`&str` 进 / 摘要 + 产物路径出。
///
/// 与 [`crate::ApprovalHandler`] / [`crate::Summarizer`] 同形：async、由上层实现、
/// 只在工具里被读。
#[async_trait]
pub trait SubagentRunner: Send + Sync {
    /// `model = None` ⇒ 用**当前**模型（缓存最暖、零额外配置）。
    /// `boundary` = 父回合的边界源；实现方**只透传 `is_aborted`，绝不 `take`**。
    ///
    /// `Err` = **产不出结果**（如 `model` 名解析失败）。**绝不静默回退到父模型** ——
    /// 用户以为换了模型其实没换，是本仓明确反对的「静默的谎」。
    /// 子 agent「跑完了但没跑好」（撞 `max_rounds` / 被取消）走 `Ok` + 非 `Completed`
    /// 的 `stop_reason`，**不是 `Err`**。
    async fn run(
        &self,
        prompt: &str,
        model: Option<&str>,
        boundary: Option<&dyn BoundarySource>,
    ) -> Result<SubagentOutcome, String>;

    /// **后台派发**：立刻返回句柄，子 agent 在后台跑；**结果会自动注入会话**。
    ///
    /// `Err` = **启动失败**（模型名解析失败 / 本回合额度用尽）。与 [`Self::run`] 同一
    /// fail-closed 纪律：绝不静默回退到父模型。
    ///
    /// # 结果怎么回来 —— **不是 poll**
    ///
    /// 实现方在完成时把结果作为 `Boundary::Deferred` **推进父回合的边界源**，
    /// `BasicLoop` 在下一个**轮边界**把它 append 进会话并 emit `DeferredResult`。
    /// 模型不必轮询、也不可能「忘记取」。
    ///
    /// # 生命周期契约（**分离任务，不与 runner 同寿**）
    ///
    /// 后台任务是 `tokio::spawn` 的**分离任务**：回合结束后仍会继续跑，结果经实现方的
    /// 跨回合投递表（coding agent 的 `PendingResults`）在后续回合送达。
    ///
    /// ⇒ **回合末不再等于任务死亡**。取消靠两层：`ChildBoundary` 透传父的 `is_aborted`、
    /// 以及子自己的预算 watchdog。
    ///
    /// 默认实现 = 不支持（返回 `Err`）⇒ 只挂同步派发的实现方不必改。
    fn spawn(
        &self,
        _prompt: &str,
        _model: Option<&str>,
        _boundary: Option<&dyn BoundarySource>,
    ) -> Result<String, String> {
        Err("本实现不支持后台派发".to_string())
    }

    /// 本**进程**范围内仍在跑的子 agent 数（ADR-0055）。
    ///
    /// # 实现约束（承重）
    ///
    /// 必须读**进程级**状态，**不得**读 runner 自己的计数器 —— runner 每回合新建，
    /// 上一回合派发、本回合仍在跑的子 agent 不在它的视野里。读自己的计数器会恒为 `0`，
    /// 让「父回合等子 agent」静默失效。
    ///
    /// # 消费者
    ///
    /// `BasicLoop` 在收场前的轮边界用它决定「能不能收场」。默认 `0` ⇒ 不挂派发的实现方
    /// 与既有测试 fake **零改动**，行为逐字节不变。
    fn running(&self) -> usize {
        0
    }

    /// 等到「[`running`](Self::running) 发生变化」—— **不分方向**，调用方必须复核。
    ///
    /// 返回 `Pin<Box<dyn Future>>` 而非 `async fn`：`ys-tool` **不带 tokio 运行时依赖**
    /// （tokio 只在 dev-dependencies），故不用 `#[async_trait]`。形状与
    /// `EventSink::emit` 同构。
    ///
    /// 默认实现永不就绪 —— 配合 `running() == 0`，调用方根本不会进入等待。
    fn settled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(std::future::pending())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 极简 fake —— 同时证明本 trait **object-safe**（能藏在 `&dyn SubagentRunner` 后面）。
    struct FakeRunner;

    #[async_trait]
    impl SubagentRunner for FakeRunner {
        async fn run(
            &self,
            prompt: &str,
            model: Option<&str>,
            boundary: Option<&dyn BoundarySource>,
        ) -> Result<SubagentOutcome, String> {
            if prompt.is_empty() {
                return Err("空任务".to_string());
            }
            let aborted = boundary.is_some_and(|b| b.is_aborted());
            Ok(SubagentOutcome {
                summary: format!("{prompt} :: model={model:?} aborted={aborted}"),
                artifact: Some("/tmp/transcript.md".to_string()),
                stop_reason: StopReason::Completed,
                diagnostic: None,
                usage: Usage {
                    input_tokens: 5,
                    output_tokens: 3,
                    prompt_cache_hit_tokens: 4,
                    prompt_cache_miss_tokens: 1,
                    reasoning_tokens: None,
                },
            })
        }
    }

    #[tokio::test]
    async fn subagent_runner_is_object_safe_and_usable() {
        let fake = FakeRunner;
        let as_dyn: &dyn SubagentRunner = &fake;

        let ok = as_dyn.run("查一下 X", Some("m1"), None).await.unwrap();
        assert_eq!(ok.summary, "查一下 X :: model=Some(\"m1\") aborted=false");
        assert_eq!(ok.artifact.as_deref(), Some("/tmp/transcript.md"));
        assert_eq!(ok.usage.prompt_cache_hit_tokens, 4);

        assert_eq!(as_dyn.run("", None, None).await.unwrap_err(), "空任务");
    }

    /// runner 能看到父的 abort 探针（用真实 `QueueBoundarySource` 验证）。
    #[tokio::test]
    async fn runner_sees_parent_abort_probe() {
        let parent = ys_protocol::QueueBoundarySource::new();
        parent.push(ys_protocol::Boundary::Abort);
        let fake = FakeRunner;
        let out = fake.run("t", None, Some(&parent)).await.unwrap();
        assert!(out.summary.contains("aborted=true"));
    }
}
