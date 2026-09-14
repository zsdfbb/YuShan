//! 命令的**能力实现**（设计 §6「能力实现归 app」）。
//!
//! 这里没有 `Command` trait、没有 registry、没有 `CommandContext` —— 命令的
//! **用户可见行为**（提示什么、问什么、什么时候开浮层）全在 TUI crate
//! (`ys-tui-coding`) 里，本模块只做「收到一条 [`Request`] 之后，世界要怎么变」。
//!
//! ```text
//! UI: 输入 → parse → ┬─ Local（TUI 自己处理）
//!                    └─ Request ──信道①──► app_loop ──► 本模块的函数
//! ```
//!
//! [`Request`]: ys_protocol::Request
//!
//! # 输出契约
//!
//! 每个函数返回 `Vec<String>`（每项一行），由 `app_loop` 包成
//! [`Outbound::Output`](ys_protocol::Outbound::Output) 送进 transcript ——
//! **本模块绝不 print**：TUI 起屏后 stderr/stdout 会冲掉整屏（设计 §5）。
//! 命令期间的失败（凭证/状态落盘失败、无法构建 model）也走同一条路。

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::provider::AuthEntry;
use crate::state::{AppState, StateStore};
use crate::wiring::Wiring;

/// `/login` 的能力：按 provider 名 + api_key 登录，存 auth.json，重建 model。
///
/// # api_base 的来路（三级回退）
///
/// 1. **用户显式给的** `requested_api_base`（`/login custom` 时 UI 追问到的 URL）
/// 2. provider 目录里的 `api_base`（deepseek / minimax 有值）
/// 3. 回退 `config.api_base`（通常来自 `YUSHAN_API_BASE`）
/// 4. 都为空 → 返回一条**明确的**提示（**不静默失败**）
///
/// 「问什么」归 UI（[`ys_tui_coding::prompter::resolve_prompt`] 只对**没有内置
/// base** 的 provider 追问 URL），本函数只负责按上面的顺序解析 —— 空串一律视同
/// 未提供（不能拿一个空 base 覆盖 config 里已有的值）。
///
/// [`Request::Login`]: ys_protocol::Request::Login
pub fn login(
    config: &mut Config,
    wiring: &mut Wiring,
    state: &StateStore,
    provider_name: &str,
    api_key: &str,
    requested_api_base: Option<&str>,
) -> Vec<String> {
    let Some(provider) = config.registry.find_provider(provider_name).cloned() else {
        let names: Vec<String> = config
            .registry
            .providers()
            .iter()
            .map(|p| p.name.clone())
            .collect();
        return vec![format!(
            "Unknown provider: {provider_name}. Available: {}",
            names.join(", ")
        )];
    };

    let api_base = requested_api_base
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| Some(provider.api_base.clone()).filter(|b| !b.is_empty()))
        .or_else(|| config.api_base.clone().filter(|b| !b.is_empty()));

    let Some(api_base) = api_base else {
        return vec![
            format!("Provider `{provider_name}` has no API base."),
            "Please provide the API base URL when logging in, or set YUSHAN_API_BASE and retry (deepseek / minimax have built-in bases).".to_string(),
        ];
    };

    let model_name = if provider.default_model.is_empty() {
        config.model.clone()
    } else {
        provider.default_model.clone()
    };

    let mut out = Vec::new();

    // 持久化凭证（失败不致命：本次会话照样能用，只是下次启动恢复不到）。
    if let Err(e) = config.registry.save_auth(
        provider_name,
        &AuthEntry {
            api_base: api_base.clone(),
            api_key: api_key.to_string(),
            model: model_name.clone(),
        },
    ) {
        out.push(format!("Warning: Could not persist credentials: {e}"));
    }

    config.api_base = Some(api_base);
    config.api_key = Some(api_key.to_string());
    config.model = model_name.clone();
    config.provider = Some(provider_name.to_string());

    // 经 factory 重建 model 交给接线器（ADR-0010：配置归接线器）。
    match config.build_model() {
        Some(model) => wiring.set_model(Some(model)),
        None => out.push("Warning: Could not build model. Check API credentials.".to_string()),
    }

    if let Err(e) = state.save(&AppState {
        last_active_provider: Some(provider_name.to_string()),
        last_active_model: Some(model_name.clone()),
    }) {
        out.push(format!("Warning: Could not persist state: {e}"));
    }

    out.push(format!("✓ Logged in to {provider_name} ({model_name})."));
    out
}

/// `/logout`：清空当前 provider 的凭证 + config 字段 + 接线器的 model。
pub fn logout(config: &mut Config, wiring: &mut Wiring, state: &StateStore) -> Vec<String> {
    let mut out = Vec::new();

    if let Some(name) = config.provider.clone()
        && let Err(e) = config.registry.remove_auth(&name)
    {
        out.push(format!(
            "Warning: Could not remove persisted credentials: {e}"
        ));
    }

    config.api_base = None;
    config.api_key = None;
    config.provider = None;
    // 配置归接线器，不伸手进 agent（ADR-0010）。
    wiring.set_model(None);

    if let Err(e) = state.save(&AppState::default()) {
        out.push(format!("Warning: Could not persist state: {e}"));
    }

    out.push("Logged out. API credentials cleared.".to_string());
    out
}

/// `/model <name>`：切模型并重建（经 factory）。带参数的那条路径 —— 无参的
/// 「浮层选一个」在 TUI 里，选完照样发 [`Request::SetModel`]。
///
/// [`Request::SetModel`]: ys_protocol::Request::SetModel
pub fn set_model(
    config: &mut Config,
    wiring: &mut Wiring,
    state: &StateStore,
    model: &str,
) -> Vec<String> {
    config.model = model.to_string();

    let mut out = Vec::new();
    if let Err(e) = state.save(&AppState {
        last_active_provider: config.provider.clone(),
        last_active_model: Some(model.to_string()),
    }) {
        out.push(format!("Warning: Could not persist state: {e}"));
    }

    // 重建失败（缺凭证）时把接线器的 model 一并置空：否则「用户刚切的模型」
    // 与「实际在跑的模型」会悄悄不一致（旧的 `/model` 只在 stdout 里提一句，
    // 但那时 model 已经不归 agent 持有，等价性无从谈起）。
    match config.build_model() {
        Some(built) => {
            wiring.set_model(Some(built));
            out.push(format!("Model switched to: {model}"));
        }
        None => {
            wiring.set_model(None);
            out.push(format!("Model set to: {model}"));
            out.push("Note: Cannot build model. Check API credentials with /login.".to_string());
        }
    }
    out
}

/// `/new`：换会话（`Wiring::new_session`）。一次性会话返回 `None`。
pub async fn new_session(wiring: &mut Wiring) -> Vec<String> {
    match wiring.new_session().await {
        Ok(Some(path)) => vec![format!(
            "New conversation started. Session: {}",
            path.display()
        )],
        Ok(None) => vec!["New conversation started. Session cleared.".to_string()],
        Err(e) => vec![format!("Failed to start new session: {e}")],
    }
}

/// `/compact`：MVP 占位 —— 清空会话（与旧行为一致）。
pub async fn compact(wiring: &mut Wiring) -> Vec<String> {
    match wiring.clear_session().await {
        Ok(()) => vec!["Compacting... Session cleared (full compaction TBD).".to_string()],
        Err(e) => vec![format!("Failed to compact session: {e}")],
    }
}

/// `/export [path]`：把会话文件复制到目标路径。
///
/// `path` 为 `None` 时落点**由 app 决定**（设计 §6）—— 就用会话文件自身路径，
/// 即「确保会话已完整落盘」；这比让 TUI 猜一个相对 cwd 的路径更安全。
/// 一次性会话（`-p`/`--json` 之外的交互会话不会遇到，但保持一致）没有会话
/// 文件可导出，明确告知而不是静默 no-op。
///
/// **同步**（`std::fs::copy`）：会话是本地小文件，一次复制不值得为它把一个
/// `&Wiring`（非 `Sync` —— `Box<dyn Session>` 只保证 `Send`）借进 app 循环的
/// future 跨 `await`，那会让整个 app 循环不再 `Send`、无法 `spawn` 到 worker。
pub fn export(wiring: &Wiring, path: Option<PathBuf>) -> Vec<String> {
    let Some(source) = wiring.session_path().map(PathBuf::from) else {
        return vec!["Export: this session is not persisted — nothing to export.".to_string()];
    };
    let target = path.unwrap_or_else(|| source.clone());

    // **自拷贝护栏**：无参 `/export` 的落点就是会话文件本身，而 `fs::copy` 对
    // 「源 == 目标」会先把目标截断 —— 那不是导出，是把会话清空。目标与会话文件
    // 是同一个文件时，`/export` 的语义退化为「确认已落盘」。
    if is_same_file(&source, &target) {
        return vec![format!("Session already persisted at {}", source.display())];
    }

    match std::fs::copy(&source, &target) {
        Ok(bytes) => vec![format!(
            "Exported {} byte(s) to {}",
            bytes,
            target.display()
        )],
        Err(e) => vec![format!("Export failed: {e}")],
    }
}

/// 两个路径是否指向同一个文件（两边都能 canonicalize 时按真实路径比，
/// 否则退回字面比较 —— 目标还不存在时 canonicalize 会失败）。
fn is_same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(real_a), Ok(real_b)) => real_a == real_b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ys_event::CollectingSink;
    use ys_model::{Model, ModelError, ModelEventSink, ModelRequest, ModelResponse};

    /// 测试目录名：**进程内原子序号 + `process::id()`**（并行安全，血液教训）。
    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "yushan_capabilities_{tag}_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 测试 model：`build_model` 的产物，只需满足 `Model`。
    struct DummyModel;

    #[async_trait::async_trait]
    impl Model for DummyModel {
        fn model_id(&self) -> &str {
            "dummy"
        }
        async fn complete(
            &self,
            _request: ModelRequest,
            _sink: &mut dyn ModelEventSink,
        ) -> Result<ModelResponse, ModelError> {
            unimplemented!("test dummy")
        }
    }

    /// 指向临时 auth.json 的 Config（默认 provider 目录保持不变）。
    fn test_config() -> (Config, PathBuf) {
        let dir = temp_dir("auth");
        let mut config = Config::from_env().unwrap();
        config.registry.set_auth_override(dir.join("auth.json"));
        config.set_model_factory(|cfg| {
            if cfg.api_base.is_some() && cfg.api_key.is_some() {
                Some(Box::new(DummyModel))
            } else {
                None
            }
        });
        (config, dir)
    }

    fn test_state() -> (StateStore, PathBuf) {
        let dir = temp_dir("state");
        let mut store = StateStore::new();
        store.set_override(dir.join("state.json"));
        (store, dir)
    }

    fn test_wiring() -> Wiring {
        Wiring::ephemeral(None, Box::new(CollectingSink::new()))
    }

    /// `/login` 成功路径：config 三个字段被填、auth 落盘、接线器拿到 model。
    #[test]
    fn test_login_success_sets_config_wiring_and_auth_file() {
        let (mut config, dir) = test_config();
        let (state, state_dir) = test_state();
        let mut wiring = test_wiring();
        assert!(wiring.model_id().is_none());

        let out = login(
            &mut config,
            &mut wiring,
            &state,
            "deepseek",
            "sk-test",
            None,
        );

        assert_eq!(config.provider.as_deref(), Some("deepseek"));
        assert_eq!(config.api_base.as_deref(), Some("https://api.deepseek.com"));
        assert_eq!(config.api_key.as_deref(), Some("sk-test"));
        assert_eq!(config.model, "deepseek-chat");
        assert_eq!(wiring.model_id(), Some("dummy"), "应经 factory 构建 model");
        assert!(
            out.iter().any(|l| l.contains("Logged in to deepseek")),
            "{out:?}"
        );

        // 凭证真的落盘了（新 registry 重读）
        let mut reloaded = crate::provider::ProviderRegistry::new();
        reloaded.set_auth_override(dir.join("auth.json"));
        reloaded.load_auth();
        let entry = reloaded.auth_for("deepseek").expect("应已落盘");
        assert_eq!(entry.api_key, "sk-test");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// 未知 provider：给出可用列表，**不改任何状态**。
    #[test]
    fn test_login_unknown_provider_reports_and_changes_nothing() {
        let (mut config, dir) = test_config();
        let (state, state_dir) = test_state();
        let mut wiring = test_wiring();

        let out = login(&mut config, &mut wiring, &state, "nope", "sk-test", None);

        assert_eq!(out.len(), 1);
        assert!(out[0].contains("Unknown provider: nope"), "{out:?}");
        assert!(out[0].contains("deepseek"), "应列出可用 provider：{out:?}");
        assert!(config.api_key.is_none(), "不得改 config");
        assert!(config.provider.is_none());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// `custom` provider 且 `config.api_base` 为空 → 明确提示设 `YUSHAN_API_BASE`
    /// （**不静默失败**，也不写空 base 的 auth）。
    #[test]
    fn test_login_custom_without_api_base_hints_env_var() {
        let (mut config, dir) = test_config();
        config.api_base = None;
        let (state, state_dir) = test_state();
        let mut wiring = test_wiring();

        let out = login(&mut config, &mut wiring, &state, "custom", "sk-test", None);

        assert!(
            out.iter().any(|l| l.contains("YUSHAN_API_BASE")),
            "应提示环境变量：{out:?}"
        );
        assert!(config.api_key.is_none(), "不得半途写入凭证");
        assert!(!dir.join("auth.json").exists(), "不得落盘半截凭证");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// `custom` provider 回退到 `config.api_base`（来自 `YUSHAN_API_BASE`）。
    #[test]
    fn test_login_custom_falls_back_to_config_api_base() {
        let (mut config, dir) = test_config();
        config.api_base = Some("https://custom.example.com".into());
        let (state, state_dir) = test_state();
        let mut wiring = test_wiring();

        let out = login(&mut config, &mut wiring, &state, "custom", "sk-test", None);

        assert_eq!(
            config.api_base.as_deref(),
            Some("https://custom.example.com")
        );
        assert_eq!(wiring.model_id(), Some("dummy"));
        assert!(out.iter().any(|l| l.contains("✓ Logged in")), "{out:?}");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// **回归**：`custom` provider 由 UI 追问到的 URL（`requested_api_base`）
    /// 必须优先于 `config.api_base` —— 否则「TUI 里填的 URL」会被旧的
    /// `YUSHAN_API_BASE` 悄悄盖掉。
    #[test]
    fn test_login_custom_uses_requested_api_base_over_config() {
        let (mut config, dir) = test_config();
        config.api_base = Some("https://stale-from-env.example.com".into());
        let (state, state_dir) = test_state();
        let mut wiring = test_wiring();

        let out = login(
            &mut config,
            &mut wiring,
            &state,
            "custom",
            "sk-test",
            Some("https://typed-in-tui.example.com/v1"),
        );

        assert_eq!(
            config.api_base.as_deref(),
            Some("https://typed-in-tui.example.com/v1"),
            "用户显式填的 URL 优先于环境变量"
        );
        assert_eq!(wiring.model_id(), Some("dummy"), "应能据此构建 model");
        assert!(out.iter().any(|l| l.contains("✓ Logged in")), "{out:?}");

        // 落盘的也是用户填的那个 URL
        let mut reloaded = crate::provider::ProviderRegistry::new();
        reloaded.set_auth_override(dir.join("auth.json"));
        reloaded.load_auth();
        assert_eq!(
            reloaded.auth_for("custom").map(|e| e.api_base.as_str()),
            Some("https://typed-in-tui.example.com/v1")
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// 空白串的 `requested_api_base` 视同「未提供」—— 不得拿空 base 覆盖内置 base。
    #[test]
    fn test_login_blank_requested_api_base_falls_back_to_builtin() {
        let (mut config, dir) = test_config();
        let (state, state_dir) = test_state();
        let mut wiring = test_wiring();

        let out = login(
            &mut config,
            &mut wiring,
            &state,
            "deepseek",
            "sk-test",
            Some("   "),
        );

        assert_eq!(
            config.api_base.as_deref(),
            Some("https://api.deepseek.com"),
            "空白 URL 不得覆盖内置 base"
        );
        assert!(out.iter().any(|l| l.contains("✓ Logged in")), "{out:?}");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// `/model <name>`：config.model 更新 + 接线器换 model + state 落盘。
    #[test]
    fn test_set_model_switches_config_wiring_and_state() {
        let (mut config, dir) = test_config();
        config.api_base = Some("https://api.deepseek.com".into());
        config.api_key = Some("sk-x".into());
        config.provider = Some("deepseek".into());
        let (state, state_dir) = test_state();
        let mut wiring = test_wiring();

        let out = set_model(&mut config, &mut wiring, &state, "deepseek-reasoner");

        assert_eq!(config.model, "deepseek-reasoner");
        assert_eq!(wiring.model_id(), Some("dummy"), "应重建并交给接线器");
        assert_eq!(
            state.load().last_active_model.as_deref(),
            Some("deepseek-reasoner")
        );
        assert!(
            out.iter().any(|l| l.contains("Model switched to")),
            "{out:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// 缺凭证时 `/model`：config 记下名字，接线器**置空**（不留下会骗人的旧模型）。
    #[test]
    fn test_set_model_without_credentials_clears_wiring_model() {
        let (mut config, dir) = test_config();
        let (state, state_dir) = test_state();
        let mut wiring =
            Wiring::ephemeral(Some(Box::new(DummyModel)), Box::new(CollectingSink::new()));
        assert!(wiring.model_id().is_some());

        let out = set_model(&mut config, &mut wiring, &state, "gpt-4o");

        assert_eq!(config.model, "gpt-4o");
        assert!(
            wiring.model_id().is_none(),
            "构建失败 → 接线器不得留着旧模型"
        );
        assert!(
            out.iter().any(|l| l.contains("Cannot build model")),
            "{out:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// `/logout`：auth 落盘被移除、config 清空、接线器 model 置空、state 复位。
    #[test]
    fn test_logout_clears_everything() {
        let (mut config, dir) = test_config();
        config.api_base = Some("https://api.deepseek.com".into());
        config.api_key = Some("sk-x".into());
        config.provider = Some("deepseek".into());
        config
            .registry
            .save_auth(
                "deepseek",
                &AuthEntry {
                    api_base: "https://api.deepseek.com".into(),
                    api_key: "sk-x".into(),
                    model: "deepseek-chat".into(),
                },
            )
            .unwrap();
        let (state, state_dir) = test_state();
        state
            .save(&AppState {
                last_active_provider: Some("deepseek".into()),
                last_active_model: Some("deepseek-chat".into()),
            })
            .unwrap();
        let mut wiring =
            Wiring::ephemeral(Some(Box::new(DummyModel)), Box::new(CollectingSink::new()));

        let out = logout(&mut config, &mut wiring, &state);

        assert!(config.api_base.is_none());
        assert!(config.api_key.is_none());
        assert!(config.provider.is_none());
        assert!(wiring.model_id().is_none());
        assert!(out.iter().any(|l| l.contains("Logged out")), "{out:?}");

        let mut reloaded = crate::provider::ProviderRegistry::new();
        reloaded.set_auth_override(dir.join("auth.json"));
        reloaded.load_auth();
        assert!(reloaded.auth_for("deepseek").is_none(), "凭证应从磁盘移除");
        assert_eq!(state.load(), AppState::default(), "state 应复位");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// `/new`（持久会话）：换到新文件，旧文件保留。
    #[tokio::test]
    async fn test_new_session_switches_session_file() {
        use ys_session::JsonlSession;

        let dir = temp_dir("newsession");
        let mut wiring = Wiring::persistent(None, Box::new(CollectingSink::new()), dir.clone())
            .await
            .unwrap();
        let old = wiring.session_path().unwrap().to_path_buf();
        wiring
            .ports()
            .session
            .append(ys_core::Message {
                role: ys_core::Role::User,
                content: vec![ys_core::ContentBlock::Text { text: "hi".into() }],
            })
            .await
            .unwrap();

        let out = new_session(&mut wiring).await;

        let new = wiring.session_path().unwrap().to_path_buf();
        assert_ne!(old, new);
        assert!(old.exists(), "旧会话文件保留");
        assert!(new.exists(), "/new 应立即落盘空文件");
        assert!(wiring.session_messages().is_empty());
        assert!(out[0].contains("New conversation started"), "{out:?}");
        // 新文件确实是可读的 jsonl（空）
        assert!(JsonlSession::open(&new).await.is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `/compact`：清空当前会话（文件仍在，内容为空）。
    #[tokio::test]
    async fn test_compact_clears_session() {
        let dir = temp_dir("compact");
        let mut wiring = Wiring::persistent(None, Box::new(CollectingSink::new()), dir.clone())
            .await
            .unwrap();
        wiring
            .ports()
            .session
            .append(ys_core::Message {
                role: ys_core::Role::User,
                content: vec![ys_core::ContentBlock::Text { text: "hi".into() }],
            })
            .await
            .unwrap();

        let out = compact(&mut wiring).await;

        assert!(wiring.session_messages().is_empty());
        assert!(out[0].contains("Session cleared"), "{out:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `/export <path>`：会话文件被复制到目标路径。
    #[tokio::test]
    async fn test_export_copies_session_file_to_target() {
        let dir = temp_dir("export");
        let mut wiring = Wiring::persistent(None, Box::new(CollectingSink::new()), dir.clone())
            .await
            .unwrap();
        wiring
            .ports()
            .session
            .append(ys_core::Message {
                role: ys_core::Role::User,
                content: vec![ys_core::ContentBlock::Text {
                    text: "exported".into(),
                }],
            })
            .await
            .unwrap();

        let target = dir.join("out.jsonl");
        let out = export(&wiring, Some(target.clone()));

        assert!(out[0].contains("Exported"), "{out:?}");
        let copied = std::fs::read_to_string(&target).unwrap();
        assert!(copied.contains("exported"), "{copied}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 无 path → 落点即会话文件自身。**不得自拷**：`fs::copy` 对「源 == 目标」
    /// 会先截断目标 —— 那不是导出，是把会话清空。
    ///
    /// 变异：去掉 `is_same_file` 护栏 → 下面的 `contains("kept")` 变红（文件被清空）。
    #[tokio::test]
    async fn test_export_without_path_does_not_truncate_session_file() {
        let dir = temp_dir("export_default");
        let mut wiring = Wiring::persistent(None, Box::new(CollectingSink::new()), dir.clone())
            .await
            .unwrap();
        // `JsonlSession` 惰性落盘：先写一条，文件才真实存在。
        wiring
            .ports()
            .session
            .append(ys_core::Message {
                role: ys_core::Role::User,
                content: vec![ys_core::ContentBlock::Text {
                    text: "kept".into(),
                }],
            })
            .await
            .unwrap();
        let path = wiring.session_path().unwrap().to_path_buf();
        assert!(path.exists());

        let out = export(&wiring, None);

        assert!(out[0].contains(&path.display().to_string()), "{out:?}");
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("kept"),
            "自拷贝会把会话文件清空：{out:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 显式把目标指成会话文件自身时，同样走护栏（不截断）。
    #[tokio::test]
    async fn test_export_to_session_path_itself_is_guarded() {
        let dir = temp_dir("export_self");
        let mut wiring = Wiring::persistent(None, Box::new(CollectingSink::new()), dir.clone())
            .await
            .unwrap();
        wiring
            .ports()
            .session
            .append(ys_core::Message {
                role: ys_core::Role::User,
                content: vec![ys_core::ContentBlock::Text {
                    text: "kept".into(),
                }],
            })
            .await
            .unwrap();
        let path = wiring.session_path().unwrap().to_path_buf();

        let out = export(&wiring, Some(path.clone()));

        assert!(out[0].contains("already persisted"), "{out:?}");
        assert!(std::fs::read_to_string(&path).unwrap().contains("kept"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一次性会话没有会话文件 → 明确告知，不静默 no-op。
    #[test]
    fn test_export_ephemeral_session_says_nothing_to_export() {
        let wiring = test_wiring();
        let out = export(&wiring, None);
        assert!(out[0].contains("nothing to export"), "{out:?}");
    }
}
