//! `CodingView` 的构造（设计 §8「视图类型归产品 TUI crate」）。
//!
//! app 线程是唯一知道 `Config` / `Agent` / `Wiring` / `ProviderRegistry` 的地方，
//! 它把这些实时数据源**拍成一张快照**（[`CodingView`]）经
//! [`Outbound::View`](ys_protocol::Outbound::View) 推给 UI。
//! UI 侧不读 `Session`、不读 registry —— 数据/显示分离（设计 §4）。
//!
//! **无网络**：可用模型列表取 registry 的**静态**表（`known_models_static`）。
//! TUI 路径不得因重绘而发起 HTTP 请求（`/v1/models` 拉取曾是登录路径的行为，
//! 现在登录也不再拉取 —— 见 `capabilities::login`）。

use std::time::Instant;

use ys_runtime::Agent;
use ys_tui_coding::{CodingView, ProviderEntry};

use crate::config::Config;
use crate::provider::ProviderRegistry;
use crate::state::StateStore;
use crate::status::TurnStats;
use crate::wiring::Wiring;

/// 从实时数据源构造显示快照。
///
/// 调用时机：view₀（启动）+ 每个 `Request` 处理完之后（`app_loop`）。
pub fn build_view(
    cfg: &Config,
    agent: &Agent,
    wiring: &Wiring,
    registry: &ProviderRegistry,
    state: &StateStore,
    stats: &TurnStats,
    session_started: Instant,
) -> CodingView {
    let logged_in = registry.logged_in_providers();

    // 绑定 provider 的 model（有 auth 时）优先，否则用 config 里的当前值；
    // 它不进静态表时也补进去，否则 `/model` 浮层里选不回当前模型。
    let bound_model = cfg
        .provider
        .as_deref()
        .and_then(|name| registry.auth_for(name))
        .map(|entry| entry.model.clone())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| cfg.model.clone());

    let mut available_models: Vec<String> = ProviderRegistry::known_models_static()
        .into_iter()
        .map(|m| m.id)
        .collect();
    if !bound_model.is_empty() && !available_models.contains(&bound_model) {
        available_models.push(bound_model);
    }

    CodingView {
        cwd: cfg.cwd.clone(),
        provider: cfg.provider.clone(),
        // 实际在跑的模型（接线器持有），而非 config 里的期望值。
        model: wiring.model_id().map(String::from),
        logged_in_providers: logged_in,
        providers: registry
            .providers()
            .iter()
            .map(|p| ProviderEntry {
                name: p.name.clone(),
                // 内置 base 原样透传（空串 = `custom` 这类需要 `/login` 追问 URL 的）。
                api_base: p.api_base.clone(),
            })
            .collect(),
        available_models,
        total_input_tokens: stats.total_input_tokens,
        total_output_tokens: stats.total_output_tokens,
        turn_count: stats.turn_count,
        session_started,
        message_count: wiring.session_messages().len(),
        tools: agent.tool_names(),
        context_window: Some(agent.context_window()),
        session_path: wiring.session_path().map(ToOwned::to_owned),
        // 首次运行 = 没有任何 auth 且没有 state.json（欢迎语用）。
        is_first_run: registry.logged_in_providers().is_empty() && !state.exists(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ys_event::CollectingSink;
    use ys_model::MockModel;

    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("yushan_view_{tag}_{}_{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_agent() -> Agent {
        ys_runtime::AgentBuilder::new().build().unwrap()
    }

    /// 无凭证、无 state：字段取默认值，`is_first_run` 为真。
    #[test]
    fn test_build_view_defaults_without_credentials() {
        let dir = temp_dir("defaults");
        let mut cfg = Config::from_env().unwrap();
        cfg.api_base = None;
        cfg.api_key = None;
        cfg.model = "deepseek-chat".into();
        cfg.cwd = PathBuf::from("/tmp/proj");
        cfg.provider = None;
        let mut state = StateStore::new();
        state.set_override(dir.join("state.json"));
        let wiring = Wiring::ephemeral(None, Box::new(CollectingSink::new()));
        let agent = test_agent();

        let view = build_view(
            &cfg,
            &agent,
            &wiring,
            &cfg.registry,
            &state,
            &TurnStats::default(),
            Instant::now(),
        );

        assert_eq!(view.cwd, PathBuf::from("/tmp/proj"));
        assert!(view.provider.is_none());
        assert!(view.model.is_none(), "无 model → 状态行显示 (no model)");
        assert!(view.logged_in_providers.is_empty());
        assert_eq!(view.providers.len(), 3, "内置三个 provider");
        assert!(view.provider_names().contains(&"deepseek".to_string()));
        // **回归锚点**：内置 base 必须原样透传 —— `/login custom` 要靠这个空串
        // 判定「该追问 API base URL」。把 api_base 拍成空 → 本断言变红。
        assert!(
            view.provider_needs_api_base_url("custom"),
            "custom 无内置 base → /login 应追问 URL：{:?}",
            view.providers
        );
        assert!(
            !view.provider_needs_api_base_url("deepseek"),
            "deepseek 有内置 base → /login 不该多问一句"
        );
        assert!(view.available_models.contains(&"deepseek-chat".to_string()));
        assert_eq!(view.turn_count, 0);
        assert_eq!(view.message_count, 0);
        assert!(view.session_path.is_none(), "一次性会话无文件");
        assert!(view.is_first_run);
        assert_eq!(view.context_window, Some(agent.context_window()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 统计与工具透传：`TurnStats` 与 `Agent::tool_names` 原样进快照。
    #[test]
    fn test_build_view_passes_stats_tools_and_session_path() {
        let dir = temp_dir("stats");
        let mut stats = TurnStats::default();
        stats.record(&ys_core::Usage {
            input_tokens: 120,
            output_tokens: 45,
        });
        stats.record(&ys_core::Usage {
            input_tokens: 1,
            output_tokens: 2,
        });

        let cfg = Config::from_env().unwrap();
        let mut state = StateStore::new();
        state.set_override(dir.join("state.json"));
        let wiring = Wiring::ephemeral(None, Box::new(CollectingSink::new()));
        let agent = ys_runtime::AgentBuilder::new()
            .tool(ys_tools_basic::ReadTool::new(PathBuf::from(".")))
            .build()
            .unwrap();
        let started = Instant::now();

        let view = build_view(
            &cfg,
            &agent,
            &wiring,
            &cfg.registry,
            &state,
            &stats,
            started,
        );

        assert_eq!(view.total_input_tokens, 121);
        assert_eq!(view.total_output_tokens, 47);
        assert_eq!(view.turn_count, 2);
        assert_eq!(view.tools, vec!["read".to_string()]);
        assert_eq!(view.session_started, started);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 已登录 provider 的 auth model 被补进 `available_models`（即使不在静态表）。
    #[tokio::test]
    async fn test_build_view_includes_bound_provider_model() {
        use crate::provider::AuthEntry;

        let dir = temp_dir("models");
        let mut cfg = Config::from_env().unwrap();
        cfg.provider = Some("deepseek".into());
        cfg.model = "some-other-model".into();
        cfg.registry.set_auth_override(dir.join("auth.json"));
        cfg.registry
            .save_auth(
                "deepseek",
                &AuthEntry {
                    api_base: "https://api.deepseek.com".into(),
                    api_key: "sk-x".into(),
                    model: "deepseek-reasoner".into(),
                },
            )
            .unwrap();

        let mut state = StateStore::new();
        state.set_override(dir.join("state.json"));
        // 持久会话：session_path 应被填进快照。
        let wiring = Wiring::persistent(
            Some(Box::new(MockModel::new("deepseek-reasoner"))),
            Box::new(CollectingSink::new()),
            dir.join("sessions"),
        )
        .await
        .unwrap();

        let view = build_view(
            &cfg,
            &test_agent(),
            &wiring,
            &cfg.registry,
            &state,
            &TurnStats::default(),
            Instant::now(),
        );

        assert!(
            view.available_models
                .contains(&"deepseek-reasoner".to_string()),
            "绑定的 auth model 应在选择器里：{:?}",
            view.available_models
        );
        assert_eq!(
            view.model.as_deref(),
            Some("deepseek-reasoner"),
            "快照里的 model 来自接线器（实际在跑的）"
        );
        assert_eq!(view.logged_in_providers, vec!["deepseek".to_string()]);
        assert!(view.session_path.is_some(), "持久会话应有会话文件");
        assert!(!view.is_first_run, "有 auth → 不是首次运行");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
