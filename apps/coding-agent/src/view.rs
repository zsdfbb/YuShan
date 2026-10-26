//! `CodingView` 的构造（设计 §8「视图类型归产品 TUI crate」）。
//!
//! app 线程是唯一知道 `Config` / `Agent` / `Wiring` / `ProviderRegistry` 的地方，
//! 它把这些实时数据源**拍成一张快照**（[`CodingView`]）经
//! [`Outbound::View`](ys_protocol::Outbound::View) 推给 UI。
//! UI 侧不读 `Session`、不读 registry —— 数据/显示分离（设计 §4）。
//!
//! **无网络**：可用模型列表取 registry 里**已经拉到**的目录
//! （`models_for_provider` / `models_for_base`，由启动期后台任务与 `/login` 填充）。
//! TUI 路径不得因重绘而发起 HTTP 请求 —— 拉取只发生在启动期后台与 `/login`。

use std::time::Instant;

use ys_runtime::Agent;

use ys_tui_coding::{CodingView, ModelEntry, ProviderEntry, ThemeKind};

use crate::config::Config;
use crate::provider::ProviderRegistry;
use crate::state::StateStore;
use crate::status::TurnStats;
use crate::wiring::Wiring;

/// 从实时数据源构造显示快照。
///
/// 调用时机：view₀（启动）+ 每个 `Request` 处理完之后（`app_loop`）。
///
/// `theme` 由调用方传入、**本函数不读盘**：`app_loop` 是在「每个请求处理完刷快照」
/// 时调它，若在这里 `state.load()` 就变成每轮对话读一次 `state.json`。取值来自
/// app 侧的**进程级镜像**（`app_loop::run` 里的 `let mut theme: ThemeKind`，
/// `Request::SetTheme` 单向更新）。
///
/// 与 `thinking_visible` 的区别正在这里：后者在 app 侧只作持久化镜像、不需要进
/// 视图（回合路径直接用局部量）；主题**必须**进视图（UI 要它上色），故只能走参数
/// 或读盘 —— 参数是正确选择。
///
/// 形参已达 8 个。**不再加参数**：若将来还有「每请求都要刷新的单个偏好」，
/// 应连同 `stats` / `session_started` / `theme` 一起收进一个 `ViewInputs` 结构体，
/// 而不是继续拉长签名。
#[allow(clippy::too_many_arguments)]
pub fn build_view(
    cfg: &Config,
    agent: &Agent,
    wiring: &Wiring,
    registry: &ProviderRegistry,
    state: &StateStore,
    stats: &TurnStats,
    session_started: Instant,
    theme: ThemeKind,
) -> CodingView {
    let logged_in = registry.logged_in_providers();

    // `/model` 无参浮层的候选 = **所有已登录 provider 已拉到的目录**，每条带自己的归属。
    //
    // **没有静态回退**：模型名只有一个来源 —— 该端点的 `/v1/models`（启动期后台拉一次、
    // `/login` 时再拉一次该 provider 的）。没拉到就是没拉到（`/model` 那时只剩当前在跑
    // 的那一条），**绝不拿写死的名字充数** —— 写死的名字必然过时，选中即 400。
    //
    // env 直连（`YUSHAN_API_BASE` + `YUSHAN_API_KEY`）没有 provider 名，按 base 取目录，
    // 归属记为「未知」（`None`，浮层里显示裸模型名）。
    //
    // **去重键是 `(provider, model)` 而非 `model`**：两个 provider 的目录里可能有**逐字
    // 同名**的 id（网关常转售别家品牌的模型名，如 opencode-go 的 `deepseek-*`），按 id
    // 去重会把其中一条**吃掉** —— 用户既看不到它、也选不到它。同名不同源各占一行，
    // 由 label 的 provider 前缀区分。
    let mut available_models: Vec<ModelEntry> = Vec::new();
    for name in &logged_in {
        for id in registry.models_for_provider(name) {
            push_model(&mut available_models, Some(name.clone()), id, false);
        }
    }
    if cfg.provider.is_none()
        && let Some(base) = cfg.api_base.as_deref()
    {
        for id in registry.models_for_base(base) {
            push_model(&mut available_models, None, id, false);
        }
    }

    // **当前在跑的模型必须在候选里**，否则切走就选不回来。
    //
    // 取据是「**接线器实际持有的 model**」+ `config.model`，而**不是** auth entry
    // 里的 `model` —— 后者是 `/login` 当时的快照，`/model X` 只改 config/wiring、
    // 从不回头改 auth entry（拿它当「当前模型」会把正在跑的那个漏掉）。
    //
    // 归属取 `cfg.provider`（**权威**）——接线器里那个 model 就是用它这套凭证建出来的。
    // 刻意**不**用 `provider_for_model` 反查：它遍历 `model_cache`（`HashMap`，迭代顺序
    // 不确定），同名 id 会反查到任一家。
    let running = wiring.model_id().map(String::from);
    for m in [running.clone(), Some(cfg.model.clone())]
        .into_iter()
        .flatten()
    {
        let current = running.as_deref() == Some(m.as_str());
        push_model(&mut available_models, cfg.provider.clone(), m, current);
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
        executed: stats.executed,
        message_count: wiring.session_messages().len(),
        tools: agent.tool_names(),
        // skill 候选：**只透传三个显示字段**（`command` / `label` / `description`）。
        // 路径与正文是 app 侧的事（`/skill-name` 的正文由 app 读盘后进 transcript），
        // TUI 不读盘 —— 数据/显示分离。
        //
        // **准入过滤发生在这里**：`config.skills` 保留全部条目（含 `user_invocable:
        // false` 的「只给模型」skill —— `expand_invocation` 自己会判），但**下发给
        // TUI 的只有可被用户调用的那些**。设计契约：`user-invocable: false` ⇒ 只给
        // 模型、不给用户敲，不得出现在 `/` 补全、`/help`、`/status`。
        //
        // 为什么过滤落在 app 侧：`ys_tui_coding::SkillEntry` 刻意只有三个显示字段，
        // 不携带 `user_invocable`（数据/显示分离，UI 不做准入决策）。若不过滤，不可
        // 调用的 skill 会出现在候选里，用户敲下它时 `expand_invocation` 拒绝展开，
        // 整条消息**原样发回模型** —— 静默降级。
        skills: cfg
            .skills
            .iter()
            .filter(|s| s.user_invocable)
            .map(|s| ys_tui_coding::SkillEntry {
                command: s.command.clone(),
                label: s.label.clone(),
                description: s.description.clone(),
            })
            .collect(),
        // R3 的可见性出口：预算安全阀把一部分 skill 从**模型**目录里收起时，
        // 用户仍要在 `/status` 看得见（默认预算下恒为 0）。
        skills_omitted: cfg.skills_omitted,
        context_window: Some(agent.context_window()),
        session_path: wiring.session_path().map(ToOwned::to_owned),
        // 首次运行 = 没有任何 auth 且没有 state.json（欢迎语用）。
        is_first_run: registry.logged_in_providers().is_empty() && !state.exists(),
        // `/thinking` 显示开关的**启动初值**（resume 时取自 meta；`App::new` 消费一次）。
        thinking_visible: wiring.meta().map(|m| m.thinking_visible).unwrap_or(false),
        // 主题的**启动初值**（来自 `state.json`；`App::new` 消费一次）。
        theme,
        // `/effort` 的当前有效档位：会话覆盖优先，否则配置默认（默认 `high`）。
        effort: cfg.effective_effort_level(),
    }
}

/// 往 `/model` 候选里加一条：按 **`(provider, model)`** 判重。
///
/// 重复时**只升 `current` 标记**、不新增行 —— 「当前正在跑的模型」与某个 provider
/// 目录里的同一条会被走到两次，那时它应当只有一行、且带上标记。
///
/// 变异：把判重换成只比 `model` ⇒ 同名跨 provider 的两条被合成一条
/// （`test_build_view_same_model_id_under_two_providers_gets_two_rows` 变红）。
fn push_model(out: &mut Vec<ModelEntry>, provider: Option<String>, model: String, current: bool) {
    if model.is_empty() {
        return;
    }
    if let Some(existing) = out
        .iter_mut()
        .find(|e| e.provider == provider && e.model == model)
    {
        existing.current |= current;
        return;
    }
    out.push(ModelEntry {
        provider,
        model,
        current,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ys_event::CollectingSink;
    use ys_model::MockModel;

    /// 候选里是否含该模型 id。
    ///
    /// 断言多数只关心「这个 id 在不在」；**归属**另有专门用例
    /// （`test_build_view_same_model_id_under_two_providers_gets_two_rows`）。
    fn has_model(view: &CodingView, id: &str) -> bool {
        view.available_models.iter().any(|c| c.model == id)
    }

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
            ThemeKind::default(),
        );

        assert_eq!(view.cwd, PathBuf::from("/tmp/proj"));
        assert!(view.provider.is_none());
        assert!(view.model.is_none(), "无 model → 状态行显示 (no model)");
        assert!(view.logged_in_providers.is_empty());
        assert_eq!(
            view.providers.len(),
            6,
            "内置 provider 全部进 /login 选择器"
        );
        assert!(view.provider_names().contains(&"deepseek".to_string()));
        assert!(
            view.provider_names().contains(&"opencode-go".to_string()),
            "opencode-go 必须出现在 /login 的 provider 选择器里"
        );
        // GLM 两个端点（通用 / 编码）都在注册表里，且各自带内置 base ——
        // 有内置 base 的 provider，`/login` 不该再多问一句 URL。
        for name in ["glm", "glm-coding"] {
            assert!(
                view.provider_names().contains(&name.to_string()),
                "{name} 必须在 /login 的 provider 选择器里：{:?}",
                view.provider_names()
            );
            assert!(
                !view.provider_needs_api_base_url(name),
                "{name} 有内置 base → /login 不该多问一句"
            );
        }
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
        // 无凭证、无目录：候选里只剩**当前在跑的**那条（`cfg.model`），
        // 不再有写死的静态表条目。
        assert!(has_model(&view, "deepseek-chat"));
        assert_eq!(view.turn_count, 0);
        assert_eq!(view.message_count, 0);
        assert!(view.session_path.is_none(), "一次性会话无文件");
        assert!(view.is_first_run);
        assert_eq!(view.context_window, Some(agent.context_window()));
        assert_eq!(
            view.effort,
            Some(ys_core::EffortLevel::High),
            "未覆盖时视图显示配置默认 high"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **C8 端到端**：`state.json` 的 `theme` → `CodingView.theme` → `App.theme`。
    ///
    /// 这条钉住持久化闭环的两端：app 侧把 kebab-case 名解析成 `ThemeKind` 塞进视图，
    /// UI 侧 `App::new` 消费一次作为运行期初值。任一端断开（例如忘了传参、或
    /// `App::new` 仍用 `ThemeKind::default()` 占位）都会红。
    ///
    /// 变异：`build_view` 里把 `theme` 参数改成 `ThemeKind::default()` ⇒ 红；
    /// `App::new` 里把 `let theme = view.theme;` 改回 `ThemeKind::default()` ⇒ 红。
    #[test]
    fn test_state_theme_reaches_view_and_app() {
        let dir = temp_dir("theme_e2e");
        let path = dir.join("state.json");
        let mut state = StateStore::new();
        state.set_override(path.clone());
        let mut st = state.load();
        st.theme = Some("nord".into());
        state.save(&st).unwrap();

        // 与启动期同一条解析路径（main.rs 的 `saved_state.theme`）。
        let theme: ThemeKind = state
            .load()
            .theme
            .as_deref()
            .and_then(|s| s.parse::<ThemeKind>().ok())
            .unwrap_or_default();
        assert_eq!(
            theme,
            ThemeKind::Nord,
            "前置：state.json 的名字应解析成 Nord"
        );

        let cfg = Config::from_env().unwrap();
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
            theme,
        );
        assert_eq!(view.theme, ThemeKind::Nord, "视图必须带上持久化的主题");

        // UI 侧消费一次作为运行期初值 —— 该断言在 `ys-tui-coding` 的 `app.rs` 里
        // （`App.theme` 是 crate 内可见，跨 crate 读不到）：
        // `test_app_new_consumes_view_theme_initial`。

        // 未知名回落默认（不 panic、不污染）。
        let mut st2 = state.load();
        st2.theme = Some("no-such-theme".into());
        state.save(&st2).unwrap();
        let fallback: ThemeKind = state
            .load()
            .theme
            .as_deref()
            .and_then(|s| s.parse::<ThemeKind>().ok())
            .unwrap_or_default();
        assert_eq!(fallback, ThemeKind::default(), "未知名应回落默认主题");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `CodingView.effort` 透传有效档位（会话覆盖优先于配置默认）。
    ///
    /// 变异：把 `CodingView { effort: cfg.effective_effort_level() }` 改回常量
    /// `None` ⇒ 本用例变红。
    #[test]
    fn test_build_view_passes_effort() {
        let dir = temp_dir("effort");
        let mut cfg = Config::from_env().unwrap();
        cfg.effort = Some(ys_core::EffortLevel::High);
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
            ThemeKind::default(),
        );

        assert_eq!(view.effort, Some(ys_core::EffortLevel::High));

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
            ..Default::default()
        });
        stats.record(&ys_core::Usage {
            input_tokens: 1,
            output_tokens: 2,
            ..Default::default()
        });
        // 执行时长也要透传（**两个口径各自独立**）。
        stats.record_elapsed(std::time::Duration::from_secs(95));

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
            ThemeKind::default(),
        );

        assert_eq!(view.total_input_tokens, 121);
        assert_eq!(view.total_output_tokens, 47);
        assert_eq!(view.turn_count, 2);
        assert_eq!(view.tools, vec!["read".to_string()]);
        assert_eq!(view.session_started, started);
        assert_eq!(
            view.executed,
            std::time::Duration::from_secs(95),
            "执行时长必须从 TurnStats 透传到快照（否则 /status 永远显示 0）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 当前正在跑的模型被补进 `available_models`（即便所在 provider 的目录还没拉回来）。
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
        let wiring = Wiring::persistent_new(
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
            ThemeKind::default(),
        );

        assert!(
            has_model(&view, "deepseek-reasoner"),
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

    /// **回归**：当前**正在跑**的模型必须始终在候选里。
    ///
    /// 场景：minimax 登录时 auth entry 的 model 是 `MiniMax-Text-01`，用户
    /// `/model MiniMax-M3` 切过去 —— `/model X` 只改 config/wiring，**从不回头改
    /// auth entry**。若「要不要补进候选」的判据取自 auth entry 的 model（改前的
    /// 写法），补进去的就是登录时的旧值，正在跑的 MiniMax-M3 反而选不回来。
    ///
    /// 变异：把 `build_view` 里补当前模型那段改回读 `auth_for(provider).model`
    /// → 本用例变红（列表里只有 MiniMax-Text-01）。
    #[tokio::test]
    async fn test_build_view_always_includes_the_running_model() {
        use crate::provider::AuthEntry;

        let dir = temp_dir("running_model");
        let mut cfg = Config::from_env().unwrap();
        cfg.provider = Some("minimax".into());
        cfg.model = "MiniMax-M3".into(); // 用户刚 /model 切到的
        cfg.registry.set_auth_override(dir.join("auth.json"));
        cfg.registry
            .save_auth(
                "minimax",
                &AuthEntry {
                    api_base: "https://api.minimax.chat/v1".into(),
                    api_key: "sk-x".into(),
                    model: "MiniMax-Text-01".into(), // 登录时的旧值
                },
            )
            .unwrap();

        let mut state = StateStore::new();
        state.set_override(dir.join("state.json"));
        let wiring = Wiring::persistent_new(
            Some(Box::new(MockModel::new("MiniMax-M3"))),
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
            ThemeKind::default(),
        );

        assert!(
            has_model(&view, "MiniMax-M3"),
            "正在跑的模型必须在候选里：{:?}",
            view.available_models
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **回归（用户报的「候选不全」）**：启动期拉到的目录覆盖**所有已登录
    /// provider**，而不只是当前那一个。
    ///
    /// 变异：把 `build_view` 的填充循环改成只看 `cfg.provider` 一个 → 未绑定的
    /// 那个 provider 的模型消失，本用例变红。
    #[tokio::test]
    async fn test_build_view_unions_models_of_all_logged_in_providers() {
        use crate::provider::AuthEntry;

        let dir = temp_dir("union");
        let mut cfg = Config::from_env().unwrap();
        cfg.provider = Some("deepseek".into());
        cfg.model = "deepseek-v4".into();
        cfg.registry.set_auth_override(dir.join("auth.json"));
        for (name, base, model) in [
            ("deepseek", "https://api.deepseek.com", "deepseek-v4"),
            ("minimax", "https://api.minimax.chat/v1", "MiniMax-M3"),
        ] {
            cfg.registry
                .save_auth(
                    name,
                    &AuthEntry {
                        api_base: base.into(),
                        api_key: format!("sk-{name}"),
                        model: model.into(),
                    },
                )
                .unwrap();
        }
        // 后台拉到的两个目录（真实路径是 `apply_model_cache` 写缓存）。
        cfg.registry
            .seed_cached_models("https://api.deepseek.com", &["deepseek-v4"]);
        cfg.registry.seed_cached_models(
            "https://api.minimax.chat/v1",
            &["MiniMax-M3", "MiniMax-M2.7"],
        );

        let mut state = StateStore::new();
        state.set_override(dir.join("state.json"));
        let wiring = Wiring::persistent_new(
            Some(Box::new(MockModel::new("deepseek-v4"))),
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
            ThemeKind::default(),
        );

        for expected in ["deepseek-v4", "MiniMax-M3", "MiniMax-M2.7"] {
            assert!(
                has_model(&view, expected),
                "候选里缺 `{expected}`（应含所有已登录 provider 的目录）：{:?}",
                view.available_models
            );
        }
        // 模型名只有目录一个来源 ⇒ 拉到了就不该再出现别的（写死的）名字。
        assert!(
            !has_model(&view, "deepseek-chat"),
            "有目录时不得再掺静态条目：{:?}",
            view.available_models
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **种子目录进 `/model` 候选**：GLM 两个端点的模型名来自 registry 的内置种子
    /// （`CatalogSource::Seed`，构造时已写进 `model_cache`），无需任何网络。
    ///
    /// 准入前提是**已登录** —— `available_models` 只收已登录 provider 的目录
    /// （未登录时那一家的模型名无处可用：没有凭证就发不出请求）。故本用例先
    /// `save_auth`，再断言候选。
    ///
    /// 变异：把 `build_view` 的填充循环改成只看 `Endpoint` 目录（或去掉种子预热）
    /// ⇒ 本用例变红。
    #[test]
    fn test_build_view_glm_seed_catalog_enters_model_candidates() {
        use crate::provider::AuthEntry;

        let dir = temp_dir("glm_seed");
        let mut cfg = Config::from_env().unwrap();
        cfg.api_base = None;
        cfg.api_key = None;
        cfg.provider = Some("glm".into());
        cfg.model = "glm-5.3".into();
        cfg.registry.set_auth_override(dir.join("auth.json"));
        // base 逐字取内置值（种子就是按这个 base 预热的）。
        for name in ["glm", "glm-coding"] {
            let base = cfg
                .registry
                .find_provider(name)
                .expect("内置 provider")
                .api_base
                .clone();
            cfg.registry
                .save_auth(
                    name,
                    &AuthEntry {
                        api_base: base,
                        api_key: "sk-glm".into(),
                        model: "glm-5.3".into(),
                    },
                )
                .unwrap();
        }

        let mut state = StateStore::new();
        state.set_override(dir.join("state.json"));
        let wiring = Wiring::ephemeral(None, Box::new(CollectingSink::new()));

        let view = build_view(
            &cfg,
            &test_agent(),
            &wiring,
            &cfg.registry,
            &state,
            &TurnStats::default(),
            Instant::now(),
            ThemeKind::default(),
        );

        for id in ["glm-5.3", "glm-5.3-flash"] {
            assert!(
                has_model(&view, id),
                "GLM 种子目录应进 /model 候选：{id} 缺失 {:?}",
                view.available_models
            );
        }
        // 两个 GLM 端点目录相同 ⇒ 同名各占一行（靠 label 的 provider 前缀区分）。
        let rows: Vec<_> = view
            .available_models
            .iter()
            .filter(|c| c.model == "glm-5.3-flash")
            .collect();
        assert_eq!(
            rows.len(),
            2,
            "glm 与 glm-coding 的目录同名，应各占一行：{:?}",
            view.available_models
        );
        // 窗口随种子落地（厂商未提供列表接口，这个值是目录里的唯一口径）。
        assert_eq!(
            cfg.registry.catalog_context_window("glm-5.3"),
            Some(1_000_000),
            "种子必须带上下文窗口"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **本需求的动机场景**：两个 provider 的目录里出现**逐字同名**的 id
    /// （网关转售别家品牌的模型名时的真实形态 —— 如 opencode-go 目录里的 `deepseek-*`）。
    ///
    /// 候选必须**各占一行**：靠 label 的 provider 前缀区分，否则其中一条既看不到也选不到。
    /// 且归属由条目自带，选中后不必再靠 `provider_for_model` 猜（那会遍历 `HashMap`，
    /// 顺序不确定 ⇒ 可能把端点静默切到错的一家）。
    ///
    /// 变异：把 `push_model` 的判重键改回只比 `model` ⇒ 只剩一行 ⇒ 本用例变红
    /// （这正是改动前 `build_view` 的旧行为：`available_models.contains(&id)`）。
    #[tokio::test]
    async fn test_build_view_same_model_id_under_two_providers_gets_two_rows() {
        use crate::provider::AuthEntry;

        let dir = temp_dir("same_id");
        let mut cfg = Config::from_env().unwrap();
        cfg.provider = Some("deepseek".into());
        cfg.model = "shared-flash".into();
        cfg.registry.set_auth_override(dir.join("auth.json"));
        for (name, base) in [
            ("deepseek", "https://api.deepseek.com"),
            ("opencode-go", "https://opencode.ai/zen/go/v1"),
        ] {
            cfg.registry
                .save_auth(
                    name,
                    &AuthEntry {
                        api_base: base.into(),
                        api_key: format!("sk-{name}"),
                        model: "shared-flash".into(),
                    },
                )
                .unwrap();
        }
        // **同一个 id 挂在两个 base 下。**
        cfg.registry
            .seed_cached_models("https://api.deepseek.com", &["shared-flash"]);
        cfg.registry
            .seed_cached_models("https://opencode.ai/zen/go/v1", &["shared-flash"]);

        let mut state = StateStore::new();
        state.set_override(dir.join("state.json"));
        let wiring = Wiring::persistent_new(
            Some(Box::new(MockModel::new("shared-flash"))),
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
            ThemeKind::default(),
        );

        let rows: Vec<_> = view
            .available_models
            .iter()
            .filter(|c| c.model == "shared-flash")
            .collect();
        assert_eq!(
            rows.len(),
            2,
            "同名跨 provider 必须各占一行，否则有一条选不到：{:?}",
            view.available_models
        );
        let providers: Vec<&str> = rows.iter().filter_map(|c| c.provider.as_deref()).collect();
        for expected in ["deepseek", "opencode-go"] {
            assert!(
                providers.contains(&expected),
                "缺 provider `{expected}`：{providers:?}"
            );
        }
        // 「正在跑的那条」与目录里的同一条是**同一行**（不新增），且标记只落在它身上。
        let current: Vec<&ModelEntry> =
            view.available_models.iter().filter(|c| c.current).collect();
        assert_eq!(current.len(), 1, "current 只该有一条：{current:?}");
        assert_eq!(current[0].provider.as_deref(), Some("deepseek"));
        assert!(
            current[0].label().ends_with(" [current]"),
            "label 须带标记：{}",
            current[0].label()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 归属未知的模型（env 直连 / 目录里没有的手敲名）：`provider == None`，
    /// label 退化为裸模型名 —— app 侧据此走反查（既有语义）。
    ///
    /// 变异：把 `push_model(…, cfg.provider.clone(), …)` 改成用 `provider_for_model`
    /// 反查 ⇒ 目录里的 `my-local-model` 若恰好挂别家就会被错标 ⇒ 「provider == None」变红。
    #[tokio::test]
    async fn test_build_view_env_direct_running_model_has_no_provider() {
        let dir = temp_dir("no_provider");
        let mut cfg = Config::from_env().unwrap();
        cfg.provider = None;
        cfg.model = "my-local-model".into();
        cfg.registry.set_auth_override(dir.join("auth.json"));

        let mut state = StateStore::new();
        state.set_override(dir.join("state.json"));
        let wiring = Wiring::persistent_new(
            Some(Box::new(MockModel::new("my-local-model"))),
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
            ThemeKind::default(),
        );

        let row = view
            .available_models
            .iter()
            .find(|c| c.model == "my-local-model")
            .expect("正在跑的模型必须在候选里");
        assert_eq!(row.provider, None, "无 cfg.provider ⇒ 归属未知");
        assert_eq!(
            row.label(),
            "my-local-model [current]",
            "退化裸名（无 provider 前缀），正在跑故带标记"
        );
        assert!(
            !row.label().contains('/'),
            "无归属不得伪造 provider 前缀：{}",
            row.label()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **skill 透传（T11）**：`cfg.skills` → `CodingView.skills`（顺序与内容一致），
    /// `cfg.skills_omitted` 原样进 `skills_omitted`；且**路径不外泄**（TUI 只拿三个
    /// 显示字段，正文由 app 侧读盘）。
    ///
    /// 变异：把映射写成 `skills: Vec::new()`（忘了接）⇒ 前两条断言变红。
    #[test]
    fn test_build_view_passes_skills_and_omitted_count() {
        use crate::skills::{Scope, SkillEntry};

        let dir = temp_dir("skills");
        let mut cfg = Config::from_env().unwrap();
        cfg.cwd = PathBuf::from("/tmp/proj");
        // 刻意**逆序**喂入：透传必须保序（顺序归 `catalogue` 管，`build_view` 不重排）。
        cfg.skills = vec![
            SkillEntry {
                command: "zeta".into(),
                label: "Z 标签".into(),
                description: "Z 描述".into(),
                path: PathBuf::from("/secret/zeta/SKILL.md"),
                scope: Scope::User,
                model_visible: true,
                user_invocable: true,
            },
            SkillEntry {
                command: "alpha".into(),
                label: "A 标签".into(),
                description: "A 描述".into(),
                path: PathBuf::from("/secret/alpha/SKILL.md"),
                scope: Scope::Project,
                model_visible: false,
                user_invocable: true,
            },
        ];
        cfg.skills_omitted = 3;

        let mut state = StateStore::new();
        state.set_override(dir.join("state.json"));
        let wiring = Wiring::ephemeral(None, Box::new(CollectingSink::new()));

        let view = build_view(
            &cfg,
            &test_agent(),
            &wiring,
            &cfg.registry,
            &state,
            &TurnStats::default(),
            Instant::now(),
            ThemeKind::default(),
        );

        let carried: Vec<(String, String, String)> = view
            .skills
            .iter()
            .map(|s| (s.command.clone(), s.label.clone(), s.description.clone()))
            .collect();
        assert_eq!(
            carried,
            vec![
                (
                    "zeta".to_string(),
                    "Z 标签".to_string(),
                    "Z 描述".to_string()
                ),
                (
                    "alpha".to_string(),
                    "A 标签".to_string(),
                    "A 描述".to_string()
                ),
            ],
            "skill 的 command/label/description 必须逐条透传且保序"
        );
        // `model_visible: false` 的条目**仍进浮层候选**（只给用户敲、不给模型读）——
        // 是否可见由 `Catalogue.lines` 决定，与列表无关。
        assert!(view.skills.iter().any(|s| s.command == "alpha"));
        assert_eq!(
            view.skills_omitted, 3,
            "预算收起的计数必须透传（/status 要显示）"
        );
        // 路径不外泄：TUI 拿到的类型里根本没有这个字段。
        assert!(
            !format!("{:?}", view.skills).contains("SKILL.md"),
            "路径不得进入 UI 侧数据：{:?}",
            view.skills
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **准入契约**：`user_invocable: false` 的 skill **不得**进 `CodingView.skills`。
    ///
    /// 设计契约：`user-invocable: false` ⇒ 只给模型、不给用户敲（不出现在 `/` 补全、
    /// `/help`、`/status`）。`config.skills` 仍保留全部条目（`expand_invocation` 自己
    /// 会判），过滤只影响下发给 TUI 的快照。
    ///
    /// 变异：去掉 `build_view` 里的 `.filter(|s| s.user_invocable)` ⇒ 本用例变红
    /// （`model-only` 会漏进候选；用户敲下它时 `expand_invocation` 拒绝展开，
    /// 整条消息原样发回模型 —— 静默降级）。
    #[test]
    fn test_build_view_filters_out_non_user_invocable_skills() {
        use crate::skills::{Scope, SkillEntry};

        let dir = temp_dir("skills_invocable");
        let mut cfg = Config::from_env().unwrap();
        cfg.cwd = PathBuf::from("/tmp/proj");
        cfg.skills = vec![
            SkillEntry {
                command: "user-facing".into(),
                label: "可调用".into(),
                description: "用户能敲".into(),
                path: PathBuf::from("/secret/user/SKILL.md"),
                scope: Scope::User,
                model_visible: false,
                user_invocable: true,
            },
            SkillEntry {
                command: "model-only".into(),
                label: "只给模型".into(),
                description: "用户不可敲".into(),
                path: PathBuf::from("/secret/model/SKILL.md"),
                scope: Scope::Project,
                model_visible: true,
                user_invocable: false,
            },
        ];

        let mut state = StateStore::new();
        state.set_override(dir.join("state.json"));
        let wiring = Wiring::ephemeral(None, Box::new(CollectingSink::new()));

        let view = build_view(
            &cfg,
            &test_agent(),
            &wiring,
            &cfg.registry,
            &state,
            &TurnStats::default(),
            Instant::now(),
            ThemeKind::default(),
        );

        let commands: Vec<&str> = view.skills.iter().map(|s| s.command.as_str()).collect();
        assert_eq!(
            commands,
            vec!["user-facing"],
            "只有 user_invocable 的 skill 能进 TUI 候选：{:?}",
            view.skills
        );
        // 过滤只发生在快照上：`config.skills` 自己**不动**（`expand_invocation`
        // 依赖它拿到 model-only 的正文）。
        assert_eq!(
            cfg.skills.len(),
            2,
            "config.skills 必须保留全部条目（不过滤）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
