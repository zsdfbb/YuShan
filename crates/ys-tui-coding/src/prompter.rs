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
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;

use ys_protocol::{Request, SessionSummary};

use crate::commands::{PROMPT_PAGE_SIZE, PromptKind};
use crate::completion::{selectable_skills, skill_label};
use crate::view::{CodingView, ModelEntry};

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

/// [`resolve_prompt`] / [`resolve_session_pick`] 的结局 —— 模态问答收尾的去向。
///
/// 大多数命令问完就发一条 [`Request`]；两处例外：`/skills` 选中一条 skill 只把
/// `/<command> ` **回填到输入框**（见 [`PromptKind::PickSkill`]）；`/resume` 选中
/// **当前会话**是 [`NoOp`](Self::NoOp)（不发请求，只提示）。
#[derive(Clone, Debug, PartialEq)]
pub enum PromptOutcome {
    /// 产出一条待发的请求。
    Request(Request),
    /// 回填输入框的文本（**不发送**）—— 与 Tab 补全的 `replacement` 同形（尾随空格）。
    Prefill(String),
    /// 有意义的无操作：不发请求，但把这条提示写进 transcript。
    ///
    /// 唯一来源是 `/resume` 选中**当前会话**（见 [`resolve_session_pick`]）——
    /// 不能静默无反应，否则用户以为命令坏了。
    NoOp(String),
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
) -> Result<PromptOutcome, PromptError> {
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
            Ok(PromptOutcome::Request(Request::Login {
                provider,
                api_key,
                api_base,
            }))
        }
        // 选模型 → `SetModel { model, provider }`（**带上归属**，见下）。
        //
        // 同 [`PickSkill`](Self::PickSkill)：`Prompter::select` 只回传**显示串**，
        // 故用同一个 [`ModelEntry::label`] 反解回条目 —— 显示串必须唯一可辨，
        // 这正是 `provider/model` 前缀的第二个用途（第一个是给用户看）。反解出
        // provider 后再送进协议：app 侧因此**不必**从模型名猜归属（猜会猜错：
        // 两个 provider 的目录里可能有逐字同名的 id）。
        PromptKind::PickModel => {
            let labels: Vec<String> = view.available_models.iter().map(ModelEntry::label).collect();
            let picked = prompter.select("Select model", labels.clone(), PROMPT_PAGE_SIZE)?;
            // 反解不出 ⇒ 视图在问答期间变了这类不可能情形：当作取消，什么都不改。
            let Some(entry) = view
                .available_models
                .iter()
                .find(|e| e.label() == picked)
            else {
                return Err(PromptError::Cancelled);
            };
            if entry.model.trim().is_empty() {
                return Err(PromptError::Cancelled);
            }
            Ok(PromptOutcome::Request(Request::SetModel {
                model: entry.model.clone(),
                provider: entry.provider.clone(),
            }))
        }
        // 选 skill → 回填 `/<command> `（**不发 Request**）。
        //
        // `Prompter::select` 只回传**显示串**，故要用同一个 [`skill_label`]
        // 反解回 `command` —— 这正是那条格式由本 crate 独占的原因（补全面与
        // 选择器共用它，两边不会各写一份）。
        PromptKind::PickSkill => {
            let selectable = selectable_skills(&view.skills);
            let options: Vec<String> = selectable.iter().map(|s| skill_label(s)).collect();
            let picked = prompter.select("Select skill", options, PROMPT_PAGE_SIZE)?;
            // 选中的串必来自上面的候选（`select` 的契约：回传 `options` 里的一项），
            // 且**在候选集里反解**（不是 `view.skills`）—— 被内置命令遮蔽的 skill
            // 不该经这条路被回填进去。找不到 ⇒ 视图在问答期间变了这类不可能情形：
            // 当作取消，什么都不改。
            let Some(skill) = selectable.into_iter().find(|s| skill_label(s) == picked) else {
                return Err(PromptError::Cancelled);
            };
            Ok(PromptOutcome::Prefill(format!("/{} ", skill.command)))
        }
        // 会话候选来自 `App.pending_sessions`（与 `&CodingView` 不同源），由
        // [`resolve_session_pick`] 处理 —— 本函数拿不到候选，故不走这里。
        PromptKind::PickSession => Err(PromptError::Other(
            "PickSession 须经 resolve_session_pick 处理".into(),
        )),
    }
}

/// `/resume` 无参浮层的决策逻辑 —— **独立于 [`resolve_prompt`]**。
///
/// 与 `resolve_prompt` 分开的原因：候选会话来自调用方的 `App.pending_sessions`
/// （`Vec<SessionSummary>`），与 `&CodingView` 不同源；把会话塞进 `CodingView` 会让
/// 「一次性载荷」混进「长期快照」，也会把这条命令的分支波及另三条浮层命令与全部
/// `FakePrompter` 单测。
///
/// `current_path` 是**当前会话**的文件路径（`CodingView.session_path`）；选中它就是
/// [`PromptOutcome::NoOp`]（不发请求、明确提示）。`None` 表示当前是无文件会话。
///
/// Esc 取消（`select` 返回 [`PromptError::Cancelled`]）原样上抛，调用方**不改任何状态**。
pub fn resolve_session_pick(
    prompter: &dyn Prompter,
    sessions: &[SessionSummary],
    current_path: Option<&Path>,
) -> Result<PromptOutcome, PromptError> {
    resolve_session_pick_at(prompter, sessions, current_path, SystemTime::now())
}

/// [`resolve_session_pick`] 的实现体，`now` 由调用方给定。
///
/// 单测据此**注入固定时刻**：标签与反解共用同一个 `now`，从而消除「标签用一次
/// `now`、反解再用一次 `now`」这一隐含假设 —— 两次独立取时若跨过相对时间的档位
/// （如午夜换日），标签与反解不再逐字节相同，反解落空。生产路径恒传
/// `SystemTime::now()`（见 [`resolve_session_pick`]）。
fn resolve_session_pick_at(
    prompter: &dyn Prompter,
    sessions: &[SessionSummary],
    current_path: Option<&Path>,
    now: SystemTime,
) -> Result<PromptOutcome, PromptError> {
    // 标签**只算一次**并原样复用做反解：`select` 只回传显示串，反解必须与它逐字节
    // 相同。`now` 是本函数唯一的时刻来源，算标签与比较标签都取它 —— 不经第二次
    // `SystemTime::now()` 重算，否则跨相对时间档位时两串不再相等，反解落空。
    let labels: Vec<String> = sessions
        .iter()
        .map(|s| session_label(s, now, current_path == Some(s.path.as_path())))
        .collect();

    let picked = prompter.select("Select session", labels.clone(), PROMPT_PAGE_SIZE)?;
    // 在**候选集**里反解（而非 `sessions` 重新生成）：契约是「回传 `options` 里的一项」。
    let Some(idx) = labels.iter().position(|l| *l == picked) else {
        // 候选在问答期间变了这类不可能情形：当作取消，什么都不改。
        return Err(PromptError::Cancelled);
    };
    let chosen = &sessions[idx];
    if current_path == Some(chosen.path.as_path()) {
        return Ok(PromptOutcome::NoOp(format!(
            "已在会话 {}（无需切换）",
            chosen.id
        )));
    }
    Ok(PromptOutcome::Request(Request::ResumeSession {
        target: chosen.id.clone(),
    }))
}

/// 会话候选标签：`<id> · <title> · <相对时间>`；当前会话加 ` [current]` 后缀。
///
/// **id 必须完整入标签，且置于最前**：
/// - 完整 —— 它是反解的唯一依据（[`Prompter::select`] 只回传显示串）。会话 id 由
///   app 侧 `new_session_id()` 生成，格式为 `{unix秒}_{纳秒:09}`（如
///   `1760000000_123456789`），`sessions::list` 用 `path.file_stem()` 取回。同一**秒**
///   创建的会话共享 `{secs}_` 前缀 —— 故任何**定长**短码都会相撞，令 `position`
///   反解到错误会话；只有完整 id 唯一，标签因此唯一可反解。
/// - 在最前 —— 绘制层（`draw.rs::prompt_select`）对超宽选项做 `truncate_to_width`，
///   末尾字段最先被截掉。id 前置后，同标题会话在窄终端上仍靠 id 前缀可区分（否则
///   屏幕上的两行趋于雷同，用户可能选中错误的**行**；反解本身仍正确，因为 `select`
///   回传的是未截断串）。
///
/// `title` 由 app 侧（`sessions::derive_title`）收在 50 字符内。
fn session_label(summary: &SessionSummary, now: SystemTime, is_current: bool) -> String {
    let suffix = if is_current { " [current]" } else { "" };
    format!(
        "{} · {} · {}{suffix}",
        summary.id,
        summary.title,
        relative_time(summary.mtime, now)
    )
}

/// mtime → 相对时间的中文短语（**显示用**，未来时间按「刚刚」处理）。
fn relative_time(mtime: SystemTime, now: SystemTime) -> String {
    let secs = now
        .duration_since(mtime)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match secs {
        0..=59 => "刚刚".to_string(),
        60..=3_599 => format!("{} 分钟前", secs / 60),
        3_600..=86_399 => format!("{} 小时前", secs / 3_600),
        _ => format!("{} 天前", secs / 86_400),
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
    use crate::view::{ProviderEntry, SkillEntry};

    fn provider(name: &str, api_base: &str) -> ProviderEntry {
        ProviderEntry {
            name: name.to_string(),
            api_base: api_base.to_string(),
        }
    }

    /// 视图里的 provider **都有内置 base**（deepseek / minimax 这类）—— `/login`
    /// 不该多问 URL。
    ///
    /// 模型候选一律挂在**第一个** provider 名下（`provider/model` 标签要能反解，
    /// 故归属必须确定）。
    fn view_with(providers: &[&str], models: &[&str]) -> CodingView {
        let owner = providers.first().map(|n| n.to_string());
        CodingView {
            providers: providers
                .iter()
                .map(|n| provider(n, "https://builtin.example.com"))
                .collect(),
            available_models: models
                .iter()
                .map(|s| ModelEntry {
                    provider: owner.clone(),
                    model: s.to_string(),
                    current: false,
                })
                .collect(),
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

        let outcome = resolve_prompt(PromptKind::Login, &p, &view).expect("应产出 Request");
        assert_eq!(
            outcome,
            PromptOutcome::Request(Request::Login {
                provider: "openai".into(),
                api_key: "sk-secret".into(),
                api_base: None,
            })
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

        let outcome = resolve_prompt(PromptKind::Login, &p, &view).expect("应产出 Request");
        assert_eq!(
            outcome,
            PromptOutcome::Request(Request::Login {
                provider: "custom".into(),
                api_key: "sk-secret".into(),
                api_base: Some("https://my-llm.example.com/v1".into()),
            })
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

        let outcome = resolve_prompt(PromptKind::Login, &p, &view).expect("应产出 Request");
        assert_eq!(
            outcome,
            PromptOutcome::Request(Request::Login {
                provider: "deepseek".into(),
                api_key: "sk-secret".into(),
                api_base: None,
            })
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

    /// 浮层选中 ⇒ `SetModel { model, provider }`：**model 字段必须是裸模型名**，
    /// provider 单独带出 —— 两者都钉住「显示串（`provider/model`）→ 条目」的反解两端。
    ///
    /// 变异：把 `model: entry.model` 换回 `model: picked`（显示串）⇒ model 字段带上
    /// `deepseek/` 前缀 ⇒ 本用例变红；丢掉 `provider` ⇒ 同样变红。
    #[test]
    fn test_pick_model_selects_from_view_models() {
        let view = view_with(&["deepseek"], &["deepseek-chat", "gpt-4o"]);
        let p = FakePrompter::with_answers(vec![Ok("deepseek/gpt-4o".into())]);

        let outcome = resolve_prompt(PromptKind::PickModel, &p, &view).expect("应产出 Request");
        assert_eq!(
            outcome,
            PromptOutcome::Request(Request::SetModel {
                model: "gpt-4o".into(),
                provider: Some("deepseek".into()),
            })
        );
        assert_eq!(
            p.calls(),
            vec!["select:Select model[deepseek/deepseek-chat,deepseek/gpt-4o]".to_string()]
        );
    }

    /// 归属未知的条目（env 直连这类）：显示裸模型名，选中也不带 provider ——
    /// app 侧因此会走反查（既有语义）。
    #[test]
    fn test_pick_model_without_provider_yields_none() {
        let view = CodingView {
            available_models: vec![ModelEntry {
                provider: None,
                model: "gw/model".into(),
                current: true,
            }],
            ..CodingView::for_test()
        };
        let p = FakePrompter::with_answers(vec![Ok("gw/model [current]".into())]);

        let outcome = resolve_prompt(PromptKind::PickModel, &p, &view).expect("应产出 Request");
        assert_eq!(
            outcome,
            PromptOutcome::Request(Request::SetModel {
                model: "gw/model".into(),
                provider: None,
            })
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

    // -----------------------------------------------------------------------
    // /skills 无参 → 浮层；选中**回填输入框**（不发 Request）
    // -----------------------------------------------------------------------

    /// 选中一条 skill ⇒ `Prefill("/<command> ")` —— **尾随空格**是承重的：
    /// 用户接着敲参数，不必自己补一个空格（与补全 `replacement` 同形）。
    ///
    /// 断言用的是**显示串**（`FakePrompter` 只认这个），故本测试同时钉住
    /// 「选项文本 = `skill_label`」与「反解回 command」两端 —— 变异：把
    /// `skill_label` 改成不列 description、或把 `command` 与 `label` 弄反 → 红。
    #[test]
    fn test_pick_skill_prefills_slash_command_with_trailing_space() {
        let view = CodingView::for_test();
        let p = FakePrompter::ok(["/commit  — 生成一条提交信息"]);

        let outcome = resolve_prompt(PromptKind::PickSkill, &p, &view).expect("应产出回填");
        assert_eq!(outcome, PromptOutcome::Prefill("/commit ".into()));
        assert_eq!(
            p.calls(),
            vec![
                "select:Select skill[/commit  — 生成一条提交信息,/review  — 审查当前改动]"
                    .to_string()
            ],
            "选项 = `/command  — description`，按 command 升序"
        );
    }

    /// **被内置命令遮蔽的 skill 不进候选**（内置 > skill，它敲下去也永远走不到）——
    /// 与 `/` 补全浮层同一条规则（共用 [`selectable_skills`]）。
    ///
    /// 变异：去掉 `selectable_skills` 里那条 filter（或让它与 `parse` 的硬序不一致）
    /// → 本测试的 `calls()` 断言变红。
    #[test]
    fn test_pick_skill_excludes_skill_shadowed_by_builtin_command() {
        let view = CodingView {
            skills: vec![
                SkillEntry {
                    command: "commit".into(),
                    label: "Git Commit".into(),
                    description: "生成一条提交信息".into(),
                },
                // 与内置命令同名 ⇒ 用户敲 `/status` 永远走内置，这条不可达。
                SkillEntry {
                    command: "status".into(),
                    label: "Shadow".into(),
                    description: "一条想遮蔽 /status 的 skill".into(),
                },
            ],
            ..CodingView::for_test()
        };
        let p = FakePrompter::ok(["/commit  — 生成一条提交信息"]);

        let outcome = resolve_prompt(PromptKind::PickSkill, &p, &view).expect("应产出回填");
        assert_eq!(outcome, PromptOutcome::Prefill("/commit ".into()));
        let calls = p.calls();
        assert!(
            calls[0].contains("/commit") && !calls[0].contains("/status"),
            "被遮蔽的 skill 不该进候选：{calls:?}"
        );
    }

    /// 取消 ⇒ 什么都不做（不产出回填、不产生 `Request`）。
    #[test]
    fn test_pick_skill_cancelled_produces_nothing() {
        let view = CodingView::for_test();
        let p = FakePrompter::with_answers(vec![]);
        assert_eq!(
            resolve_prompt(PromptKind::PickSkill, &p, &view),
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

    // -----------------------------------------------------------------------
    // /resume 无参 → 浮层（resolve_session_pick，候选来自 App.pending_sessions）
    // -----------------------------------------------------------------------

    use std::time::{Duration, UNIX_EPOCH};

    fn summary(id: &str, title: &str, mtime_secs: u64) -> SessionSummary {
        SessionSummary {
            id: id.into(),
            path: std::path::PathBuf::from(format!("/sessions/{id}.jsonl")),
            mtime: UNIX_EPOCH + Duration::from_secs(mtime_secs),
            title: title.into(),
        }
    }

    /// **同标题不切错**（本单元最要紧的一条）：两个 title 都是「新会话」的不同 id，
    /// 标签靠**完整 id** 区分 ⇒ 反解出被选中的那一个。
    ///
    /// **时刻固定**（`resolve_session_pick_at`）：标签与反解共用同一个注入的 `now`，
    /// 不依赖「两次 `SystemTime::now()` 相同」——否则两次调用跨相对时间档位（如午夜
    /// 换日）时标签与反解串不再相等，反解落空而误红。
    ///
    /// 变异：把 `session_label` 换成只放「id 前 8 字符」的短码（两 id 同前缀）⇒
    /// 候选标签不再唯一，反解落到错误会话（或第一个），本用例变红。
    #[test]
    fn test_session_pick_resolves_correct_session_among_same_titles() {
        let sessions = vec![
            summary("1760000000_000000001", "新会话", 1_760_000_000),
            summary("1760000000_000000002", "新会话", 1_760_000_100),
        ];
        let now = UNIX_EPOCH + Duration::from_secs(1_760_000_200);
        // 选中 B —— 用 FakePrompter 回传 B 的显示串（与 resolve 共用同一个 now）。
        let label_b = session_label(&sessions[1], now, false);
        let p = FakePrompter::with_answers(vec![Ok(label_b)]);

        let outcome = resolve_session_pick_at(&p, &sessions, None, now)
            .expect("选中一个会话应产出请求");
        assert_eq!(
            outcome,
            PromptOutcome::Request(Request::ResumeSession {
                target: "1760000000_000000002".into(),
            }),
            "同标题下必须反解到被选中的 id"
        );
    }

    /// **P2-3**：id 前置 —— 窄终端下（绘制层 `draw.rs::prompt_select` 对超宽选项做
    /// 前缀截断）两个同标题会话仍可区分。截到「只够放下 id」的宽度时两串必须不同。
    ///
    /// 变异：把 id 挪回末尾 ⇒ ① id 前置断言红；② 前缀截断先切掉 id，两串趋同 ⇒
    /// 可区分断言红。
    #[test]
    fn test_session_label_id_leads_so_narrow_truncation_stays_distinguishable() {
        let a = summary("1760000000_000000001", "新会话", 1_760_000_000);
        let b = summary("1760000000_000000002", "新会话", 1_760_000_000);
        let now = UNIX_EPOCH + Duration::from_secs(1_760_000_000);
        let la = session_label(&a, now, false);
        let lb = session_label(&b, now, false);

        // id 必须出现在 title 之前（前置契约）。
        assert!(
            la.find(a.id.as_str()).unwrap() < la.find("新会话").unwrap(),
            "id 必须前置：{la}"
        );

        // 截到只够放下完整 id 的宽度（+1 容纳其后分隔符），两行仍不同。
        let width = a.id.chars().count() + 1;
        let pa: String = la.chars().take(width).collect();
        let pb: String = lb.chars().take(width).collect();
        assert_ne!(pa, pb, "窄宽前缀截断后仍须可区分：{pa:?} vs {pb:?}");
        assert!(pa.contains(a.id.as_str()), "id 应落在可见前缀内：{pa:?}");
    }

    /// 选中**当前会话** ⇒ `NoOp`（不发请求、给一句明确提示），并带 `[current]` 标记。
    ///
    /// 同 [`test_session_pick_resolves_correct_session_among_same_titles`]：注入固定
    /// `now`，消除对「两次 `SystemTime::now()` 相同」的依赖。
    #[test]
    fn test_session_pick_current_session_is_noop_with_hint() {
        let sessions = vec![
            summary("a", "会话 A", 1_760_000_000),
            summary("b", "会话 B", 1_760_000_100),
        ];
        let current = std::path::PathBuf::from("/sessions/a.jsonl");
        let now = UNIX_EPOCH + Duration::from_secs(1_760_000_200);
        let label_a = session_label(&sessions[0], now, true);
        assert!(label_a.ends_with("[current]"), "当前会话带标记：{label_a}");
        let p = FakePrompter::with_answers(vec![Ok(label_a)]);

        let outcome = resolve_session_pick_at(&p, &sessions, Some(current.as_path()), now)
            .expect("选中当前会话不应报错");
        match outcome {
            PromptOutcome::NoOp(hint) => assert!(hint.contains("无需切换"), "{hint}"),
            other => panic!("选中当前会话应为 NoOp，得到 {other:?}"),
        }
    }

    /// Esc（FakePrompter 队列空）⇒ `Err(Cancelled)` —— 调用方据此**不改任何状态**。
    #[test]
    fn test_session_pick_cancel_produces_nothing() {
        let sessions = vec![summary("a", "会话 A", 1_760_000_000)];
        let p = FakePrompter::with_answers(vec![]);
        assert_eq!(
            resolve_session_pick(&p, &sessions, None),
            Err(PromptError::Cancelled)
        );
    }

    /// 标签里含 **完整 id / title / 相对时间** 三样，`id` 前置；相对时间按区间给出中文短语。
    #[test]
    fn test_session_label_shape_and_relative_time() {
        let s = summary("1760000000_123456789", "修复 UTF-8", 1_760_000_000);
        let now = UNIX_EPOCH + Duration::from_secs(1_760_000_000 + 120);
        let label = session_label(&s, now, false);
        assert!(label.contains("修复 UTF-8"), "{label}");
        assert!(
            label.contains("1760000000_123456789"),
            "必须含完整 id 以备反解：{label}"
        );
        assert!(label.contains("2 分钟前"), "{label}");
        assert!(!label.contains("[current]"), "非当前会话无标记：{label}");
        // 顺序：id → title → 相对时间（id 前置，窄宽截断下先于 title 可见）。
        assert!(
            label.starts_with("1760000000_123456789 · 修复 UTF-8 · "),
            "id 必须前置：{label}"
        );

        let at = |secs: u64| relative_time(UNIX_EPOCH, UNIX_EPOCH + Duration::from_secs(secs));
        assert_eq!(at(30), "刚刚");
        assert_eq!(at(3_600), "1 小时前");
        assert_eq!(at(90_000), "1 天前");
        // 未来时间（时钟回拨）按「刚刚」，不 panic、不出负数
        assert_eq!(relative_time(UNIX_EPOCH + Duration::from_secs(100), UNIX_EPOCH), "刚刚");
    }
}
