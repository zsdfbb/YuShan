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

/// 一个已知 skill（`/` 补全浮层的候选 + `/status` 可列举项）。
///
/// `command` 是**目录名**（也就是 `/` 后要敲的那个词），`label` 是 frontmatter
/// 里的 `name`（**仅用于显示**，与 `command` 可以不同）。UI 不解析 skill 目录 ——
/// 它只消费 app 侧扫描好的这份快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillEntry {
    /// 目录名 —— `/` 命令名。
    pub command: String,
    /// frontmatter `name`（仅显示）。
    pub label: String,
    pub description: String,
}

/// `/model` 浮层的一个候选：`model` 是 id，`provider` 是归属。
///
/// `provider == None` 表示**归属未知** —— 只可能出现在「当前正在跑 / `cfg.model` 补进候选」
/// 那条路径上（无 `cfg.provider`，如 `YUSHAN_API_BASE` 直连的自定义端点）。那时显示退化为
/// 裸模型名，选中也不带 provider（交 app 侧按目录反查）。
///
/// **为什么不并成 `provider/model` 一个字符串**：选中后要分别送进
/// `Request::SetModel { model, provider }` —— 拼成一个串再在协议边界上拆，等于把
/// 「哪个是 provider」这个判断推给下游去猜。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelEntry {
    pub provider: Option<String>,
    pub model: String,
    /// 当前正在跑的那一条。**唯一写者**是 app 侧的 `build_view`（据接线器持有的 model）。
    pub current: bool,
}

impl ModelEntry {
    /// 浮层里的显示串 —— **它同时是反解的契约**。
    ///
    /// `Prompter::select` 只回传显示串，调用方靠字符串相等找回条目（与 `skill_label` /
    /// `session_label` 同一条纪律，见 `M10`）。故形状必须**唯一可辨**：
    /// 有归属 → `provider/model`；无归属 → 裸 `model`；`current` → 追加 ` [current]`
    /// （与 `session_label` 同形）。
    pub fn label(&self) -> String {
        let base = match &self.provider {
            Some(p) => format!("{p}/{}", self.model),
            None => self.model.clone(),
        };
        if self.current {
            format!("{base} [current]")
        } else {
            base
        }
    }
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
    /// `/model` 浮层的候选（带 provider 归属，见 [`ModelEntry`]）。
    pub available_models: Vec<ModelEntry>,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub turn_count: u32,
    /// 会话起点（`session_duration_str` 由此派生；`/new` 后换新）。
    pub session_started: Instant,
    /// 会话历史条数（含 pending）。
    pub message_count: usize,
    pub tools: Vec<String>,
    /// 已知 skill（供 `/` 补全浮层列举；同时供 `/status` 显示条数）。
    pub skills: Vec<SkillEntry>,
    /// 因整表字节预算被收起的 skill 条数（`/status` 可见；默认预算下恒为 0）。
    pub skills_omitted: usize,
    pub context_window: Option<usize>,
    pub session_path: Option<PathBuf>,
    /// 首次运行（无 `auth.json` 凭证）—— 用于欢迎语。
    pub is_first_run: bool,
    /// 会话恢复的 `/thinking` 显示开关**启动初值**。
    ///
    /// 只在启动时被 `App::new` 消费一次；运行期真值是 `App.thinking_visible`，
    /// 此后本字段仅是 app 侧持久化镜像的载体（设计 §5.6）。
    pub thinking_visible: bool,
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
            skills: Vec::new(),
            skills_omitted: 0,
            context_window: None,
            session_path: None,
            is_first_run: true,
            thinking_visible: false,
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
            available_models: vec![ModelEntry {
                provider: Some("deepseek".into()),
                model: "deepseek-chat".into(),
                current: true,
            }],
            total_input_tokens: 0,
            total_output_tokens: 0,
            turn_count: 0,
            session_started: Instant::now(),
            message_count: 0,
            tools: vec!["read".into(), "write".into()],
            // 两条 skill：`command` 与 `label` 故意不同 —— 覆盖「补全用目录名、
            // 显示用 frontmatter name」两条路径。
            skills: vec![
                SkillEntry {
                    command: "commit".into(),
                    label: "Git Commit".into(),
                    description: "生成一条提交信息".into(),
                },
                SkillEntry {
                    command: "review".into(),
                    label: "Code Review".into(),
                    description: "审查当前改动".into(),
                },
            ],
            // 非零值：`/status` 里「有 skill 被预算收起」那一行能被测试走到
            // （默认预算下真实运行恒为 0）。
            skills_omitted: 3,
            context_window: Some(65_536),
            session_path: None,
            is_first_run: false,
            thinking_visible: false,
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

    /// `Default` 的中性值：没有 skill，也没有被收起的 skill。
    #[test]
    fn test_default_view_has_no_skills() {
        let v = CodingView::default();
        assert!(v.skills.is_empty(), "Default 无 skill");
        assert_eq!(v.skills_omitted, 0, "默认预算下无收起");
    }

    /// `for_test` 字段齐全：两条 skill 的 `command`/`label` 分属两个词表，
    /// 且 `skills_omitted` 非零（让 `/status` 的「已收起 N 条」那行有数据可测）。
    #[test]
    fn test_for_test_view_carries_skills() {
        let v = CodingView::for_test();
        assert_eq!(v.skills.len(), 2, "两条构造 skill");
        assert_eq!(v.skills[0].command, "commit");
        assert_eq!(v.skills[0].label, "Git Commit", "label ≠ command");
        assert_eq!(v.skills[1].command, "review");
        assert_eq!(v.skills_omitted, 3, "非零：覆盖「已收起」显示路径");
    }

    /// `SkillEntry` 可比较（`/` 补全去重与测试断言都靠它）。
    #[test]
    fn test_skill_entry_equality() {
        let a = SkillEntry {
            command: "commit".into(),
            label: "Git Commit".into(),
            description: "d".into(),
        };
        assert_eq!(a.clone(), a);
        let b = SkillEntry {
            command: "commit".into(),
            label: "Committing".into(),
            description: "d".into(),
        };
        assert_ne!(a, b, "label 不同即不等");
    }

    /// **`ModelEntry::label` 是浮层的显示串，同时是反解的契约** —— 三种形状各钉一遍。
    ///
    /// 变异：``Some(p) => format!("{p}/{}", …)`` 改成只返回 `model.clone()`
    /// ⇒ 同名跨 provider 的两条 label 相同 ⇒ 反解只能找到第一条 ⇒ 本用例与
    /// `prompter::tests::test_pick_model_selects_from_view_models` 一起变红。
    #[test]
    fn test_model_entry_label_shapes() {
        let with_provider = ModelEntry {
            provider: Some("deepseek".into()),
            model: "deepseek-chat".into(),
            current: false,
        };
        assert_eq!(with_provider.label(), "deepseek/deepseek-chat");

        let unowned = ModelEntry {
            provider: None,
            model: "gw/model".into(),
            current: false,
        };
        assert_eq!(unowned.label(), "gw/model", "归属未知 ⇒ 裸模型名");

        let current = ModelEntry {
            current: true,
            ..with_provider.clone()
        };
        assert_eq!(current.label(), "deepseek/deepseek-chat [current]");
        assert_ne!(current.label(), with_provider.label(), "[current] 必须可辨");
    }
}
