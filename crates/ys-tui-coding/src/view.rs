//! [`CodingView`] —— UI 想显示什么的定义（设计 §8「视图类型归产品 TUI crate」）。
//!
//! 它是 **app 线程推来的快照**（`Outbound::View`），不放进 `ys-protocol`：
//! 各产品的视图字段不同（coding 看 provider/tokens/tools；投资会看净值/持仓）。
//!
//! **视图不含 `ToolCallId → ToolCall` 映射** —— 工具结果回填靠 `App` 自己的
//! `tool_index`（transcript 内的下标），视图只承载「状态行 / `/status`」要显示的东西。

use std::path::PathBuf;
use std::time::Instant;

use crate::format::format_duration;

/// 一个已知 provider（`/login` 浮层的选项）。
///
/// 它同时承载「**要不要追问 API base URL**」的判据：`api_base` 为空串的
/// provider（`custom`）需要用户自己填 URL，有内置 base 的（deepseek / minimax）
/// 不该多问一句。判据来自 app 侧的 registry —— **UI 不硬编码 provider 名单**
/// （`resolve_prompt` 只看这个字段，不认得「custom」这个名字）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderEntry {
    pub name: String,
    /// 内置 API base。**空串 = 该 provider 需要用户显式提供 URL**。
    pub api_base: String,
}

/// coding agent 的显示快照。
#[derive(Clone, Debug)]
pub struct CodingView {
    pub cwd: PathBuf,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// 已登录的 provider 名。
    pub logged_in_providers: Vec<String>,
    /// 已知 provider（供后续 `/login` 浮层选择器；同时供「要不要问 URL」判定）。
    pub providers: Vec<ProviderEntry>,
    /// 可用模型名（供后续 `/model` 浮层选择器）。
    pub available_models: Vec<String>,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub turn_count: u32,
    /// 会话起点（`session_duration_str` 由此派生；`/new` 后换新）。
    pub session_started: Instant,
    /// 会话历史条数（含 pending）。
    pub message_count: usize,
    pub tools: Vec<String>,
    pub context_window: Option<usize>,
    pub session_path: Option<PathBuf>,
    /// 首次运行（无 `auth.json` 凭证）—— 用于欢迎语。
    pub is_first_run: bool,
}

impl Default for CodingView {
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            provider: None,
            model: None,
            logged_in_providers: Vec::new(),
            providers: Vec::new(),
            available_models: Vec::new(),
            total_input_tokens: 0,
            total_output_tokens: 0,
            turn_count: 0,
            session_started: Instant::now(),
            message_count: 0,
            tools: Vec::new(),
            context_window: None,
            session_path: None,
            is_first_run: true,
        }
    }
}

impl CodingView {
    /// 已知 provider 名列表（`/status` 的 `known` 与 `/login` 选择器的选项）。
    pub fn provider_names(&self) -> Vec<String> {
        self.providers.iter().map(|p| p.name.clone()).collect()
    }

    /// 该 provider 是否**没有内置 base** —— 也就是 `/login` 时该不该追问
    /// 「API base URL」。未知 provider 一律 `false`（UI 不替 app 猜）。
    pub fn provider_needs_api_base_url(&self, name: &str) -> bool {
        self.providers
            .iter()
            .any(|p| p.name == name && p.api_base.is_empty())
    }

    /// 会话已开时长：`Ns` / `Nm Ms` / `Nh Mm`。
    ///
    /// 每次调用现算（`Instant` 无「序列化」概念，快照里的必然是起点）。
    pub fn session_duration_str(&self) -> String {
        format_duration(self.session_started.elapsed().as_secs())
    }

    /// 状态行左侧的模型段：`None` → `(no model)`。
    pub fn model_label(&self) -> String {
        self.model
            .clone()
            .unwrap_or_else(|| "(no model)".to_string())
    }

    /// 测试构造：字段齐全但值中性，避免每个测试重复 16 个字段。
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self {
            cwd: PathBuf::from("/tmp"),
            provider: Some("deepseek".into()),
            model: Some("deepseek-chat".into()),
            logged_in_providers: vec!["deepseek".into()],
            // 一个「有内置 base」+ 一个「需要用户填 URL」的 provider —— 两条
            // `/login` 追问路径都能在测试里走到（与真实 registry 同构）。
            providers: vec![
                ProviderEntry {
                    name: "deepseek".into(),
                    api_base: "https://api.deepseek.com".into(),
                },
                ProviderEntry {
                    name: "custom".into(),
                    api_base: String::new(),
                },
            ],
            available_models: vec!["deepseek-chat".into()],
            total_input_tokens: 0,
            total_output_tokens: 0,
            turn_count: 0,
            session_started: Instant::now(),
            message_count: 0,
            tools: vec!["read".into(), "write".into()],
            context_window: Some(65_536),
            session_path: None,
            is_first_run: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_default_is_neutral() {
        let v = CodingView::default();
        assert!(v.model.is_none());
        assert!(v.is_first_run);
        assert_eq!(v.session_duration_str(), "0s");
    }

    #[test]
    fn test_session_duration_str_tracks_start() {
        let v = CodingView {
            session_started: Instant::now() - Duration::from_secs(83),
            ..CodingView::default()
        };
        // 允许跨秒抖动：83~84 秒都可能
        let s = v.session_duration_str();
        assert!(s == "1m23s" || s == "1m24s", "got {s}");
    }

    #[test]
    fn test_model_label_fallback() {
        let mut v = CodingView::default();
        assert_eq!(v.model_label(), "(no model)");
        v.model = Some("gpt-4o".into());
        assert_eq!(v.model_label(), "gpt-4o");
    }

    /// `/login` 追问 URL 的判据：**只有**「已知且内置 base 为空」才追问。
    /// 未知 provider 一律不追问（UI 不替 app 猜一个 URL）。
    #[test]
    fn test_provider_needs_api_base_url_only_for_known_empty_base() {
        let v = CodingView::for_test();
        assert!(
            v.provider_needs_api_base_url("custom"),
            "custom 无内置 base → 应追问 URL"
        );
        assert!(
            !v.provider_needs_api_base_url("deepseek"),
            "有内置 base → 不得多问一句"
        );
        assert!(
            !v.provider_needs_api_base_url("nope"),
            "未知 provider → 不追问"
        );
        assert_eq!(
            v.provider_names(),
            vec!["deepseek".to_string(), "custom".to_string()]
        );
    }

    /// 还没收到任何快照（`Default`）时判据也不 panic、一律不追问。
    #[test]
    fn test_default_view_never_asks_for_api_base_url() {
        let v = CodingView::default();
        assert!(!v.provider_needs_api_base_url("custom"));
        assert!(v.provider_names().is_empty());
    }
}
