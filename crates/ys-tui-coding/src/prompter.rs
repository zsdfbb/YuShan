//! [`Prompter`] —— 「命令向用户要一个答案」的可测抽象（设计 §3）。
//!
//! 设计 §3 作废了外部交互库包装出来的那个 prompter 实现，但**保留 trait**：
//! `select` / `text` 正是「选择器可测」的答案 —— 命令的**决策逻辑**
//! （问什么、按什么顺序问、取消时怎么办）写在 [`resolve_prompt`] 里，
//! 单测注入 [`FakePrompter`] 即可覆盖全部路径；**TUI 实现**（`run.rs` 里的
//! `TuiPrompter`）只负责把同一套问答画成嵌套事件循环的模态浮层。
//!
//! ```text
//!              ┌── FakePrompter（测试注入，队列喂答案）
//! Prompter ◄───┤
//!              └── TuiPrompter（run.rs，Clear 后盖在 Chat 上，不切屏）
//! ```

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

use ys_protocol::Request;

use crate::commands::{PROMPT_PAGE_SIZE, PromptKind};
use crate::view::CodingView;

/// 一次问答的失败原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptError {
    /// 用户按 Esc（或回车交了空答案）—— **中止整条命令，不改任何状态**。
    Cancelled,
    /// 其他失败（选择器无可选项、终端 IO 出错…）。
    Other(String),
}

impl std::fmt::Display for PromptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromptError::Cancelled => write!(f, "已取消"),
            PromptError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl From<std::io::Error> for PromptError {
    fn from(e: std::io::Error) -> Self {
        PromptError::Other(e.to_string())
    }
}

/// 向用户提问的最小接口。
///
/// **同步**且 `&self`：UI 线程在这里阻塞跑模态循环；实现要内可变（TUI 侧是
/// 终端句柄，测试侧是答案队列）。
pub trait Prompter: Send + Sync {
    /// 从 `options` 里选一个（`page_size` 为一屏条数，超出滚动）。
    fn select(
        &self,
        prompt: &str,
        options: Vec<String>,
        page_size: usize,
    ) -> Result<String, PromptError>;

    /// 收一行自由文本（`help` 是可选的提示语）。
    fn text(&self, prompt: &str, help: Option<&str>) -> Result<String, PromptError>;
}

/// 把「需要模态浮层的命令」跑成一条 [`Request`] —— **纯决策逻辑，可单测**。
///
/// `/login` 的问答序列：选 provider → 问 api_key →（**仅当**该 provider 没有
/// 内置 base 时）问 API base URL。判据来自 [`CodingView`]，故 `custom` 这类
/// 需要用户填 URL 的 provider 能恢复交互式设置，而 deepseek / minimax 不多问一句。
///
/// 任一步取消（`Cancelled`）都直接上抛，**不产生任何 `Request`**：
/// 半截登录（选了 provider 却没给 key / 没给 URL）在错误的地方失败，比什么都不做更糟。
pub fn resolve_prompt(
    kind: PromptKind,
    prompter: &dyn Prompter,
    view: &CodingView,
) -> Result<Request, PromptError> {
    match kind {
        PromptKind::Login => {
            let provider =
                prompter.select("Select provider", view.provider_names(), PROMPT_PAGE_SIZE)?;
            let api_key = prompter.text("API key", Some(&provider))?;
            if api_key.trim().is_empty() {
                // 空 key 等同取消：绝不用空凭证去覆盖已有的 auth.json
                return Err(PromptError::Cancelled);
            }
            // 该 provider **没有内置 base**（`custom` 这类）→ 追问 URL。
            // 有内置 base 的（deepseek / minimax）**不**多问一句 —— 判据来自
            // 视图（app 侧的 registry），UI 不硬编码 provider 名单。
            let api_base = if view.provider_needs_api_base_url(&provider) {
                let url = prompter.text("API base URL", Some(&provider))?;
                // 空 URL 也归取消：半截登录（选了 provider、给了 key、却没有 URL）
                // 在错误的地方失败，比什么都不做更糟。
                if url.trim().is_empty() {
                    return Err(PromptError::Cancelled);
                }
                Some(url)
            } else {
                None
            };
            Ok(Request::Login {
                provider,
                api_key,
                api_base,
            })
        }
        PromptKind::PickModel => {
            let model = prompter.select(
                "Select model",
                view.available_models.clone(),
                PROMPT_PAGE_SIZE,
            )?;
            if model.trim().is_empty() {
                return Err(PromptError::Cancelled);
            }
            Ok(Request::SetModel { model })
        }
    }
}

/// 测试注入用的 [`Prompter`]：按队列顺序吐答案，队列空即 `Cancelled`。
///
/// `Mutex` 而非 `RefCell` —— trait 要求 `Send + Sync`，且实现本身要在
/// 多线程测试里被 `&dyn` 共享。
pub struct FakePrompter {
    answers: Mutex<VecDeque<Result<String, PromptError>>>,
    /// 被问到的问题（`select:Select provider` / `text:API key`），供断言顺序与文案。
    calls: Mutex<Vec<String>>,
}

impl FakePrompter {
    /// 依次作答；用尽后一律 `Err(Cancelled)`。
    pub fn with_answers(answers: Vec<Result<String, PromptError>>) -> Self {
        Self {
            answers: Mutex::new(answers.into()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// 全部答「是」的便捷构造：`FakePrompter::with_answers(vec![Ok(a), Ok(b)])`。
    pub fn ok(answers: impl IntoIterator<Item = &'static str>) -> Self {
        Self::with_answers(answers.into_iter().map(|s| Ok(s.to_string())).collect())
    }

    /// 已被问到的问题列表（按调用顺序）。
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn lock(
        m: &Mutex<VecDeque<Result<String, PromptError>>>,
    ) -> MutexGuard<'_, VecDeque<Result<String, PromptError>>> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn record(&self, entry: String) {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(entry);
    }

    fn next(&self) -> Result<String, PromptError> {
        Self::lock(&self.answers)
            .pop_front()
            .unwrap_or(Err(PromptError::Cancelled))
    }
}

impl Prompter for FakePrompter {
    fn select(
        &self,
        prompt: &str,
        options: Vec<String>,
        _page_size: usize,
    ) -> Result<String, PromptError> {
        self.record(format!("select:{prompt}[{}]", options.join(",")));
        self.next()
    }

    fn text(&self, prompt: &str, help: Option<&str>) -> Result<String, PromptError> {
        self.record(match help {
            Some(h) => format!("text:{prompt}({h})"),
            None => format!("text:{prompt}"),
        });
        self.next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::ProviderEntry;

    fn provider(name: &str, api_base: &str) -> ProviderEntry {
        ProviderEntry {
            name: name.to_string(),
            api_base: api_base.to_string(),
        }
    }

    /// 视图里的 provider **都有内置 base**（deepseek / minimax 这类）—— `/login`
    /// 不该多问 URL。
    fn view_with(providers: &[&str], models: &[&str]) -> CodingView {
        CodingView {
            providers: providers
                .iter()
                .map(|n| provider(n, "https://builtin.example.com"))
                .collect(),
            available_models: models.iter().map(|s| s.to_string()).collect(),
            ..CodingView::for_test()
        }
    }

    /// `custom`（**无内置 base**）打头 + 一个有内置 base 的作对照 ——
    /// 「问 / 不问 URL」两条路径在同一张视图里。
    fn view_with_custom() -> CodingView {
        CodingView {
            providers: vec![
                provider("custom", ""),
                provider("deepseek", "https://api.deepseek.com"),
            ],
            ..CodingView::for_test()
        }
    }

    /// 队列空 → `Cancelled`（而不是 panic / 卡住）。
    #[test]
    fn test_fake_prompter_empty_queue_cancels() {
        let p = FakePrompter::with_answers(vec![]);
        assert_eq!(
            p.select("s", vec!["a".into()], 8),
            Err(PromptError::Cancelled)
        );
        assert_eq!(p.text("t", None), Err(PromptError::Cancelled));
        assert_eq!(p.text("t", None), Err(PromptError::Cancelled));
    }

    #[test]
    fn test_fake_prompter_returns_answers_in_order() {
        let p = FakePrompter::with_answers(vec![Ok("first".into()), Ok("second".into())]);
        assert_eq!(p.text("t", None), Ok("first".into()));
        assert_eq!(p.text("t", None), Ok("second".into()));
        assert_eq!(p.text("t", None), Err(PromptError::Cancelled), "用尽即取消");
    }

    #[test]
    fn test_fake_prompter_records_calls_and_options() {
        let p = FakePrompter::ok(["x"]);
        let _ = p.select("Select model", vec!["m1".into(), "m2".into()], 8);
        assert_eq!(p.calls(), vec!["select:Select model[m1,m2]".to_string()]);
    }

    // -----------------------------------------------------------------------
    // /login 的决策逻辑
    // -----------------------------------------------------------------------

    /// 有内置 base 的 provider：**只**问 provider + api_key，`api_base` 为 `None`
    /// （交给 app 侧回退到内置 base）。
    #[test]
    fn test_login_success_produces_request_in_ask_order() {
        let view = view_with(&["deepseek", "openai"], &[]);
        let p = FakePrompter::with_answers(vec![Ok("openai".into()), Ok("sk-secret".into())]);

        let req = resolve_prompt(PromptKind::Login, &p, &view).expect("应产出 Request");
        assert_eq!(
            req,
            Request::Login {
                provider: "openai".into(),
                api_key: "sk-secret".into(),
                api_base: None,
            }
        );
        assert_eq!(
            p.calls(),
            vec![
                "select:Select provider[deepseek,openai]".to_string(),
                "text:API key(openai)".to_string(),
            ],
            "先选 provider，再问 api_key（且带上 provider 作为提示）"
        );
    }

    /// **回归**：`custom`（没有内置 base）必须追问 URL —— 否则用户只能退出重开
    /// 去设 `YUSHAN_API_BASE`（TUI 内无法补设）。
    ///
    /// 变异：删掉 `provider_needs_api_base_url` 分支 → 本测试的问答序列与
    /// `api_base` 断言同时变红。
    #[test]
    fn test_login_custom_provider_asks_for_api_base_url() {
        let view = view_with_custom();
        let p = FakePrompter::with_answers(vec![
            Ok("custom".into()),
            Ok("sk-secret".into()),
            Ok("https://my-llm.example.com/v1".into()),
        ]);

        let req = resolve_prompt(PromptKind::Login, &p, &view).expect("应产出 Request");
        assert_eq!(
            req,
            Request::Login {
                provider: "custom".into(),
                api_key: "sk-secret".into(),
                api_base: Some("https://my-llm.example.com/v1".into()),
            }
        );
        assert_eq!(
            p.calls(),
            vec![
                "select:Select provider[custom,deepseek]".to_string(),
                "text:API key(custom)".to_string(),
                "text:API base URL(custom)".to_string(),
            ],
            "custom 必须走到第三问：API base URL"
        );
    }

    /// 反向对照：有内置 base 的 provider **不**多问 —— 行为与改动前一致。
    /// （FakePrompter 里第 3 个答案故意放着，用 `calls()` 证明它**没被消费**。）
    #[test]
    fn test_login_builtin_provider_does_not_ask_for_api_base_url() {
        let view = view_with_custom();
        let p = FakePrompter::with_answers(vec![
            Ok("deepseek".into()),
            Ok("sk-secret".into()),
            Ok("https://unused.example.com".into()),
        ]);

        let req = resolve_prompt(PromptKind::Login, &p, &view).expect("应产出 Request");
        assert_eq!(
            req,
            Request::Login {
                provider: "deepseek".into(),
                api_key: "sk-secret".into(),
                api_base: None,
            }
        );
        assert_eq!(p.calls().len(), 2, "deepseek 不该被问 URL：{:?}", p.calls());
        assert_eq!(
            p.text("probe", None),
            Ok("https://unused.example.com".into()),
            "第 3 个答案应原封不动留在队列里（证明它没被 login 消费）"
        );
    }

    /// 在 API base URL 处取消 → **不产生 Request**（半截登录比什么都不做更糟）。
    #[test]
    fn test_login_cancelled_at_api_base_url_produces_nothing() {
        let view = view_with_custom();
        let p = FakePrompter::with_answers(vec![Ok("custom".into()), Ok("sk-secret".into())]);

        assert_eq!(
            resolve_prompt(PromptKind::Login, &p, &view),
            Err(PromptError::Cancelled)
        );
        assert_eq!(p.calls().len(), 3, "问到了第三步才取消：{:?}", p.calls());
    }

    /// 空 URL 等同取消（与空 key 同一道理）。
    #[test]
    fn test_login_blank_api_base_url_is_cancelled() {
        let view = view_with_custom();
        let p = FakePrompter::with_answers(vec![
            Ok("custom".into()),
            Ok("sk-secret".into()),
            Ok("   ".into()),
        ]);
        assert_eq!(
            resolve_prompt(PromptKind::Login, &p, &view),
            Err(PromptError::Cancelled)
        );
    }

    /// 在 provider 选择处取消 → **不产生 Request**，也**不问 api_key**。
    #[test]
    fn test_login_cancelled_at_provider_produces_nothing() {
        let view = view_with(&["deepseek"], &[]);
        let p = FakePrompter::with_answers(vec![]);

        assert_eq!(
            resolve_prompt(PromptKind::Login, &p, &view),
            Err(PromptError::Cancelled)
        );
        assert_eq!(p.calls().len(), 1, "取消后不得继续往下问：{:?}", p.calls());
    }

    /// 在 api_key 处取消 → **不产生 Request**（半截登录比什么都不做更糟）。
    #[test]
    fn test_login_cancelled_at_api_key_produces_nothing() {
        let view = view_with(&["deepseek", "openai"], &[]);
        let p = FakePrompter::with_answers(vec![Ok("deepseek".into())]);

        assert_eq!(
            resolve_prompt(PromptKind::Login, &p, &view),
            Err(PromptError::Cancelled)
        );
        assert_eq!(p.calls().len(), 2, "只问到第二步");
    }

    /// 交了空 key 等同取消 —— 绝不用空凭证覆盖 auth.json。
    #[test]
    fn test_login_blank_key_is_cancelled() {
        let view = view_with(&["deepseek"], &[]);
        let p = FakePrompter::with_answers(vec![Ok("deepseek".into()), Ok("   ".into())]);
        assert_eq!(
            resolve_prompt(PromptKind::Login, &p, &view),
            Err(PromptError::Cancelled)
        );
    }

    /// 非取消的错误原样上抛（调用方据此写提示）。
    #[test]
    fn test_login_other_error_propagates() {
        let view = view_with(&["deepseek"], &[]);
        let p = FakePrompter::with_answers(vec![Err(PromptError::Other("terminal gone".into()))]);
        assert_eq!(
            resolve_prompt(PromptKind::Login, &p, &view),
            Err(PromptError::Other("terminal gone".into()))
        );
    }

    // -----------------------------------------------------------------------
    // /model 无参 → 浮层
    // -----------------------------------------------------------------------

    #[test]
    fn test_pick_model_selects_from_view_models() {
        let view = view_with(&["deepseek"], &["deepseek-chat", "gpt-4o"]);
        let p = FakePrompter::with_answers(vec![Ok("gpt-4o".into())]);

        let req = resolve_prompt(PromptKind::PickModel, &p, &view).expect("应产出 Request");
        assert_eq!(
            req,
            Request::SetModel {
                model: "gpt-4o".into()
            }
        );
        assert_eq!(
            p.calls(),
            vec!["select:Select model[deepseek-chat,gpt-4o]".to_string()]
        );
    }

    #[test]
    fn test_pick_model_cancelled_produces_nothing() {
        let view = view_with(&["deepseek"], &["deepseek-chat"]);
        let p = FakePrompter::with_answers(vec![]);
        assert_eq!(
            resolve_prompt(PromptKind::PickModel, &p, &view),
            Err(PromptError::Cancelled)
        );
    }

    /// 视图里没有模型时也不会 panic —— 空列表交给 TUI 实现去报「无可选项」。
    #[test]
    fn test_pick_model_empty_list_cancels() {
        let view = view_with(&[], &[]);
        let p = FakePrompter::with_answers(vec![]);
        assert_eq!(
            resolve_prompt(PromptKind::PickModel, &p, &view),
            Err(PromptError::Cancelled)
        );
    }

    /// trait 对象可跨线程共享（`Send + Sync` 的真实用途：测试里 `&dyn` 共享）。
    #[test]
    fn test_prompter_trait_object_is_send_sync() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn Prompter>();
    }

    #[test]
    fn test_io_error_maps_to_other() {
        let e: PromptError = std::io::Error::other("boom").into();
        assert_eq!(e, PromptError::Other("boom".to_string()));
        assert_eq!(PromptError::Cancelled.to_string(), "已取消");
    }
}
