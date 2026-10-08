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

    // `/model` 无参浮层的候选 = **所有已登录 provider** 的模型并集。
    //
    // 每个 provider 的条目：优先启动期拉到的 `/v1/models` 目录（`refresh_models`
    // 写入的缓存），缺失则回退静态表中属于它的部分（`known_models_static` 的
    // `owned_by` 即 provider 名）。**未登录任何 provider** 时退回静态全表 —— 那时
    // 列出的模型本就点不动（无凭证），保留只为 `/model` 仍可浏览。
    let mut available_models: Vec<String> = Vec::new();
    if logged_in.is_empty() {
        available_models = ProviderRegistry::known_models_static()
            .into_iter()
            .map(|m| m.id)
            .collect();
    } else {
        for name in &logged_in {
            for id in registry.models_for_provider(name) {
                if !available_models.contains(&id) {
                    available_models.push(id);
                }
            }
        }
    }

    // **当前在跑的模型必须在候选里**，否则切走就选不回来。
    //
    // 取据是「**接线器实际持有的 model**」+ `config.model`，而**不是** auth entry
    // 里的 `model` —— 后者是 `/login` 当时的快照，`/model X` 只改 config/wiring、
    // 从不回头改 auth entry（拿它当「当前模型」会把正在跑的那个漏掉）。
    let running = wiring.model_id().map(String::from);
    for m in [running, Some(cfg.model.clone())].into_iter().flatten() {
        if !m.is_empty() && !available_models.contains(&m) {
            available_models.push(m);
        }
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
        );

        assert!(
            view.available_models.contains(&"MiniMax-M3".to_string()),
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
        // 启动期拉到的两个目录（真实路径是 `refresh_models` 写缓存）。
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
        );

        for expected in ["deepseek-v4", "MiniMax-M3", "MiniMax-M2.7"] {
            assert!(
                view.available_models.contains(&expected.to_string()),
                "候选里缺 `{expected}`（应含所有已登录 provider 的目录）：{:?}",
                view.available_models
            );
        }
        // 未拉到时才回退静态表；这里两个 provider 都拉到了，故不该混进静态条目。
        assert!(
            !view.available_models.contains(&"deepseek-chat".to_string()),
            "有目录时不得再掺静态条目：{:?}",
            view.available_models
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
