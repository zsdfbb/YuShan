use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;
use ys_model::Model;

use crate::provider::ProviderRegistry;

/// Model factory 函数类型。封装适配器特有的构建逻辑。
/// config 不完整（缺 api_base/api_key）时返回 None。
type ModelFactory = Box<dyn Fn(&Config) -> Option<Box<dyn Model>> + Send + Sync>;

/// `~/.yushan/config.json` 的**原始**反序列化形态（**用户手写**，故宽容解析）。
///
/// 值先落 `serde_json::Value`，再由 [`parse_user_config`] 逐项校验 —— 这样
/// **一条非法条目只丢自己**，不会把整份文件打回默认。未知字段由 serde 默认忽略
/// （向后兼容：将来加字段不影响旧文件，旧 app 读新文件也不炸）。
///
/// `#[serde(default)]` 的范式照 `ys_protocol::Request::Login.api_base`：全部字段可选。
#[derive(Debug, Default, Deserialize)]
struct RawUserConfig {
    /// model → context window（token）覆盖。**键是 model，不是 provider**。
    #[serde(default)]
    context_window: HashMap<String, serde_json::Value>,
    #[serde(default)]
    compaction: RawCompaction,
}

/// `config.json` 的 `compaction` 段。字段全可选，未给的用 `CompactPolicy::default()`。
#[derive(Debug, Default, Deserialize)]
struct RawCompaction {
    #[serde(default)]
    trigger_ratio: Option<serde_json::Value>,
    #[serde(default)]
    keep_ratio: Option<serde_json::Value>,
}

/// **已校验**的用户配置覆盖项（`config.json` 的规整形态）。
///
/// 与 [`RawUserConfig`] 分开：读一次、校验一次，解析路径（[`Config::context_window`]）
/// 只管查表，不再重复做类型判断。
#[derive(Debug, Clone, Default)]
struct UserConfig {
    /// model → context window（仅正整数）
    windows: HashMap<String, usize>,
    /// 触发比例覆盖（仅有限值且落 `(0, 1]`）
    trigger_ratio: Option<f64>,
    /// 保留比例覆盖（同上）
    keep_ratio: Option<f64>,
}

pub struct Config {
    pub api_base: Option<String>,
    pub api_key: Option<String>,
    pub model: String,
    pub cwd: PathBuf,
    pub provider: Option<String>,
    pub registry: ProviderRegistry,
    /// 启动期扫描到的 skill（**已去重、已按 `command` 升序**）。
    ///
    /// 由 [`Config::load_skills`] 写入。三处消费者：系统提示的目录段（`main` 经
    /// [`Config::load_skills`] 的返回值之外的 `skills::catalogue`）、`/` 浮层的候选
    /// （`view::build_view` 只取 `command`/`label`/`description`，**路径不外泄**）、
    /// `Request::Prompt` 的 `/skill-name` 展开（`app_loop`）。
    ///
    /// ⚠️ **绝不写进 `state.json`** —— 那是 `StateStore::save()` 会**覆盖**的文件。
    pub skills: Vec<crate::skills::SkillEntry>,
    /// 因整表字节预算被收起的 skill 条数（`Catalogue.omitted`，R3 的可见性出口）。
    pub skills_omitted: usize,
    /// 从 `~/.yushan/config.json` 读到的覆盖项（用户写、app 只读）。
    ///
    /// 由 [`Config::load_user_config`] 写入，经 [`Config::context_window`] /
    /// [`Config::compact_policy`] 读出（`main.rs` 装配进 `RunLimits`）。
    user_config: UserConfig,
    model_factory: Option<ModelFactory>,
}

impl Config {
    /// 从环境变量加载。除 `cwd` 外所有字段都是可选的。
    /// agent 可在无 API 凭证时启动；通过 `/login` 配置。
    pub fn from_env() -> Result<Self, String> {
        let api_base = std::env::var("YUSHAN_API_BASE")
            .or_else(|_| std::env::var("OPENAI_API_BASE"))
            .ok();
        let api_key = std::env::var("YUSHAN_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .ok();
        let model = std::env::var("YUSHAN_MODEL").unwrap_or_else(|_| "deepseek-chat".into());
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;

        Ok(Self {
            api_base,
            api_key,
            model,
            cwd,
            provider: None,
            registry: ProviderRegistry::new(),
            skills: Vec::new(),
            skills_omitted: 0,
            user_config: UserConfig::default(),
            model_factory: None,
        })
    }

    pub fn is_configured(&self) -> bool {
        self.api_base.is_some() && self.api_key.is_some()
    }

    /// 解析当前 model 的 context window（三级）：
    ///
    /// ① `~/.yushan/config.json` 里该 model 的覆盖（最具体，用户说了算）
    /// ② 内置表 [`crate::provider::builtin_context_window`]
    /// ③ `RunLimits::default().context_window` = 128_000（兜底）
    ///
    /// **每回合都要重算**：`/login`、`/model` 会在运行中换 model，只在启动时算一次
    /// 会让「登到 1M 模型后仍按 128k 压缩」的 bug 复活。
    ///
    /// 生产调用点：`main.rs::resolve_limits`（启动初值）与 `app_loop`（每回合刷新）。
    pub fn context_window(&self) -> usize {
        if let Some(&window) = self.user_config.windows.get(&self.model) {
            return window;
        }
        if let Some(window) = crate::provider::builtin_context_window(&self.model) {
            return window;
        }
        ys_component::RunLimits::default().context_window
    }

    /// 压缩策略：`config.json` 的比例覆盖叠在 [`ys_component::CompactPolicy::default`] 上。
    /// 只给 `trigger_ratio` 时 `keep_ratio` 仍是默认值（逐字段独立）。
    pub fn compact_policy(&self) -> ys_component::CompactPolicy {
        let mut policy = ys_component::CompactPolicy::default();
        if let Some(ratio) = self.user_config.trigger_ratio {
            policy.trigger_ratio = ratio;
        }
        if let Some(ratio) = self.user_config.keep_ratio {
            policy.keep_ratio = ratio;
        }
        policy
    }

    /// 读 `~/.yushan/config.json`（**用户写、app 只读**）。
    ///
    /// 容错三档，**绝不阻断启动**、**绝不 panic**：
    /// - 文件不存在 → 什么都不做（全默认），**不落日志**（照
    ///   [`crate::provider::ProviderRegistry::load_auth`] 的惯例：用户没写配置文件
    ///   是常态，不是故障）
    /// - 整份畸形 → 落日志后**整份忽略**，沿用调用前的状态
    /// - 单条值非法 → 只丢该条（落日志），同文件其余条目照常生效
    ///
    /// 接**显式 `path`** 而非内部拼 `$HOME`：测试可直接传临时文件，**不必改写 `HOME`**
    /// —— 与 `test_env::env_lock` 的竞争面就此消失。
    ///
    /// ⚠️ **不要**把这份配置写进 `~/.yushan/state.json` —— 那是
    /// `StateStore::save()` 会**覆盖**的文件（`state.rs`）。
    pub fn load_user_config(&mut self, path: &std::path::Path) {
        let data = match std::fs::read_to_string(path) {
            Ok(data) => data,
            Err(_) => return,
        };
        match parse_user_config(&data) {
            Ok(user) => self.user_config = user,
            // 库内部诊断 → 日志文件（设计 §5）：启动期 TUI 可能已起屏，
            // stderr 会冲掉整屏，而这里没有「历史输出」可看。
            Err(e) => crate::logging::log(&format!("failed to parse config.json: {e}")),
        }
    }

    /// 默认用户配置路径：`~/.yushan/config.json`（无 HOME 时为 `None`）。
    pub fn default_user_config_path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
        Some(PathBuf::from(home).join(".yushan").join("config.json"))
    }

    /// skill 扫描根：`[<cwd>/.yushan/skills, <home>/.yushan/skills]`。
    ///
    /// # 顺序即语义（**不是**风格问题）
    ///
    /// [`crate::skills::scan`] 靠**位置**判定 `scope`：`roots[0]` ⇒ `Scope::Project`、
    /// `roots[1..]` ⇒ `Scope::User`，而同名（命令名 = 目录名）时 **project 胜**（整条
    /// 替换）。故**项目级必须在前** —— 顺序错了就是静默的语义错误（项目本地覆盖失效，
    /// 用户级反而遮蔽项目级）。本函数是该顺序在生产侧的**唯一来源**。
    ///
    /// 无 `HOME` / `USERPROFILE` 时只返回项目那一条（照
    /// [`Config::default_user_config_path`] 的读法）。**不扫 `.claude/skills`**
    /// （设计 D2；JackCC 靠软链进 `~/.yushan/skills`）。
    ///
    /// # 去重（`cwd == $HOME` 时）
    ///
    /// 从 home 目录启动时两条根算出来是**同一路径**。若照发两条，[`crate::skills::scan`]
    /// 会把每个 skill 收两遍（一条 `Project`、一条 `User`，`path` 相同），于是
    /// `shadow_diagnostics` 对**每个** skill 产一条**假的**「project 级遮蔽 user 级」
    /// 诊断 —— 几十行纯噪音恰好淹没 R4（「一个仓库能静默换掉你的 `/git-status`」）依赖
    /// 的那个信号，启动期 IO 也翻倍。故两条路径相等时只返回一条。
    ///
    /// ⚠️ 注意：**不得**为此改 [`crate::skills::scan`] 的 scope-by-position 契约 ——
    /// 去重只发生在「同一路径」这一退化情形，三条以上的根顺序语义原样保留。
    pub fn default_skill_roots(cwd: &std::path::Path) -> Vec<PathBuf> {
        let project = cwd.join(".yushan").join("skills");
        let mut roots = vec![project.clone()];
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            let user = PathBuf::from(home).join(".yushan").join("skills");
            if user != project {
                roots.push(user);
            }
        }
        roots
    }

    /// 扫描 skill 根：条目落 `self.skills`、预算收束计数落 `self.skills_omitted`，
    /// **诊断作为返回值**交给调用方打印（本函数**绝不 print** —— 与
    /// `capabilities.rs` / `toolset.rs` 同规）。
    ///
    /// 入参走[`Config::default_skill_roots`]给出的显式路径（`scan` **不读 env**）。
    /// 空目录 / 目录不存在 ⇒ 零条目、零诊断（静默，不是故障）。
    ///
    /// **不写盘**：`skills` 是纯内存快照，绝不进 `~/.yushan/state.json`
    /// （那是 app 会 `save()` 覆盖的文件）。
    pub fn load_skills(&mut self, roots: &[PathBuf]) -> Vec<String> {
        let (entries, diags) = crate::skills::scan(roots);
        let catalogue =
            crate::skills::catalogue(entries, crate::skills::Budget::default());
        self.skills_omitted = catalogue.omitted;
        self.skills = catalogue.entries;
        diags
    }

    /// 设置 model factory。适配器类型确定后在 main.rs 中调用一次。
    pub fn set_model_factory(
        &mut self,
        factory: impl Fn(&Config) -> Option<Box<dyn Model>> + Send + Sync + 'static,
    ) {
        self.model_factory = Some(Box::new(factory));
    }

    /// 从当前 config 构建 model。委托给 factory。
    pub fn build_model(&self) -> Option<Box<dyn Model>> {
        self.model_factory.as_ref().and_then(|f| f(self))
    }

    /// 返回当前 provider 的 ProviderCompat（未设置时为 "custom"）。
    pub fn current_compat(&self) -> ys_model_openai_compat::compat::ProviderCompat {
        let name = self.provider.as_deref().unwrap_or("custom");
        self.registry.compat_for(name)
    }
}

/// 解析 + 校验 `config.json` 文本。`Err` 仅表示**整份** JSON 畸形；
/// 单条非法由本函数**就地丢弃 + 落日志**，不进 `Err`。
fn parse_user_config(data: &str) -> Result<UserConfig, serde_json::Error> {
    let raw: RawUserConfig = serde_json::from_str(data)?;
    let mut user = UserConfig::default();

    for (model, value) in raw.context_window {
        // `as_u64` 同时挡下负值、浮点与字符串；0 另有语义（窗口为 0 会让压缩退化），
        // 一并按非法处理。
        match value.as_u64().and_then(|n| usize::try_from(n).ok()) {
            Some(window) if window > 0 => {
                user.windows.insert(model, window);
            }
            _ => crate::logging::log(&format!(
                "ignoring config.json context_window[\"{model}\"]: \
                 expected a positive integer, got {value}"
            )),
        }
    }

    user.trigger_ratio = sanitize_ratio("trigger_ratio", raw.compaction.trigger_ratio);
    user.keep_ratio = sanitize_ratio("keep_ratio", raw.compaction.keep_ratio);
    Ok(user)
}

/// 比例校验：必须是**有限**值且落 `(0, 1]`。`None` 表示「未给 / 非法」→ 用默认值。
///
/// 上界取 1.0 而非 `< 1.0`：`trigger_ratio = 1.0`（用满窗口才压）是合法意图，
/// 且 `CompactPolicy::trigger_at` 自己会用 `output_reserve` 钳住，不会溢出。
fn sanitize_ratio(key: &str, value: Option<serde_json::Value>) -> Option<f64> {
    let value = value?;
    match value.as_f64() {
        Some(ratio) if ratio.is_finite() && ratio > 0.0 && ratio <= 1.0 => Some(ratio),
        _ => {
            crate::logging::log(&format!(
                "ignoring config.json compaction.{key}: \
                 expected a ratio in (0, 1], got {value}"
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::{EnvRestore, env_lock};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 直接构造 `Config`（不经 `from_env`）：本模块的配置解析测试**不读环境变量**，
    /// 因而不必持 `env_lock` —— 与改写 env 的测试零竞争。
    fn test_config(model: &str) -> Config {
        Config {
            api_base: None,
            api_key: None,
            model: model.to_string(),
            cwd: PathBuf::from("/tmp"),
            provider: None,
            registry: ProviderRegistry::new(),
            skills: Vec::new(),
            skills_omitted: 0,
            user_config: UserConfig::default(),
            model_factory: None,
        }
    }

    /// 临时目录名含**进程内原子序号 + `process::id()`** —— 并行测试不得撞名
    /// （`SystemTime::as_nanos()` 在 macOS 只到微秒级，会撞）。
    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("yushan_config_{name}_{}_{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 把一段 JSON 写进临时 config.json，返回 (目录, 文件路径)。调用方负责删目录。
    fn write_config(name: &str, content: &str) -> (PathBuf, PathBuf) {
        let dir = temp_dir(name);
        let path = dir.join("config.json");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    /// `Config::from_env` 读取的 API 凭证相关环境变量。
    const API_ENV_KEYS: &[&str] = &[
        "YUSHAN_API_BASE",
        "YUSHAN_API_KEY",
        "OPENAI_API_BASE",
        "OPENAI_API_KEY",
    ];

    /// 清空 [`API_ENV_KEYS`]，返回 Drop 时自动恢复原值的 guard。
    /// 调用方必须先持有 [`env_lock`]。
    fn clear_api_env() -> EnvRestore {
        let env = EnvRestore::capture(API_ENV_KEYS);
        // SAFETY: 调用方持有 env_lock，独占这些变量的修改与恢复。
        unsafe {
            for key in API_ENV_KEYS {
                std::env::remove_var(key);
            }
        }
        env
    }

    // 测试用的 dummy model
    struct DummyModel;

    #[async_trait::async_trait]
    impl ys_model::Model for DummyModel {
        fn model_id(&self) -> &str {
            "dummy"
        }
        async fn complete(
            &self,
            _request: ys_model::ModelRequest,
            _sink: &mut dyn ys_model::ModelEventSink,
        ) -> Result<ys_model::ModelResponse, ys_model::ModelError> {
            unimplemented!("test dummy")
        }
    }

    #[test]
    fn test_config_from_env_no_vars() {
        let _guard = env_lock();
        let _env = clear_api_env();

        let config = Config::from_env().unwrap();
        assert!(!config.is_configured());
        assert!(config.build_model().is_none());
        assert!(config.registry.find_provider("deepseek").is_some());
    }

    #[test]
    fn test_config_model_factory() {
        let _guard = env_lock();
        let _env = clear_api_env();

        let mut config = Config::from_env().unwrap();
        assert!(config.build_model().is_none());

        // 用凭证设置 config
        config.api_base = Some("https://api.example.com".into());
        config.api_key = Some("sk-test".into());

        // 没有 factory 时仍返回 None
        assert!(config.build_model().is_none());

        // 注册 factory
        config.set_model_factory(|cfg| {
            if cfg.api_base.is_some() && cfg.api_key.is_some() {
                Some(Box::new(DummyModel))
            } else {
                None
            }
        });

        assert!(config.build_model().is_some());
    }

    #[test]
    fn test_config_current_compat() {
        // from_env 会读 API_ENV_KEYS，与改写这些 key 的测试共享同一把锁。
        let _guard = env_lock();
        let mut config = Config::from_env().unwrap();
        config.provider = Some("deepseek".into());
        let compat = config.current_compat();
        assert!(compat.has_reasoning_content);
    }

    #[test]
    fn test_config_current_compat_unknown() {
        let _guard = env_lock();
        let config = Config::from_env().unwrap();
        // 未设置 provider -> "custom" -> standard
        let compat = config.current_compat();
        assert!(!compat.has_reasoning_content);
        assert!(!compat.tool_calls_as_text);
    }

    #[test]
    fn test_startup_recovery_from_auth() {
        let _guard = env_lock();

        // 准备一个含 deepseek 凭证的临时 auth.json
        let dir = std::env::temp_dir().join("yushan_test_startup_recovery");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let auth_path = dir.join("auth.json");

        let auth_data = serde_json::json!({
            "deepseek": {
                "api_base": "https://api.deepseek.com",
                "api_key": "sk-test-recovery",
                "model": "deepseek-chat"
            }
        });
        std::fs::write(
            &auth_path,
            serde_json::to_string_pretty(&auth_data).unwrap(),
        )
        .unwrap();

        // 用无环境变量（未配置）的方式创建 Config
        let _env = clear_api_env();

        let mut config = Config::from_env().unwrap();
        config.registry.set_auth_override(auth_path);
        config.registry.load_auth();

        // 模拟 main.rs 的恢复循环
        if !config.is_configured() {
            for provider in config.registry.providers() {
                if let Some(entry) = config.registry.auth_for(&provider.name) {
                    config.api_base = Some(entry.api_base.clone());
                    config.api_key = Some(entry.api_key.clone());
                    config.model = entry.model.clone();
                    config.provider = Some(provider.name.clone());
                    break;
                }
            }
        }

        assert!(config.is_configured());
        assert_eq!(config.api_base.as_deref(), Some("https://api.deepseek.com"));
        assert_eq!(config.api_key.as_deref(), Some("sk-test-recovery"));
        assert_eq!(config.model, "deepseek-chat");
        assert_eq!(config.provider.as_deref(), Some("deepseek"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- context_window 三级解析 + config.json ----

    /// **三级优先级逐级命中**（本刀的核心不变量）。
    ///
    /// 三个探针的选取都是「能区分相邻两级」的，不是随手挑的：
    /// - `MiniMax-M3` 内置 1M，config.json 覆盖 555_000 ⇒ **① 压过 ②**
    /// - `MiniMax-M2.7` 只在内置表（204_800 ≠ 128_000 默认）⇒ **② 压过 ③**
    /// - `unknown-model` 两处都无 ⇒ **③ 兜底 128_000**
    ///
    /// 变异自检：把 `context_window()` 里 ① ② 的顺序倒过来（内置表盖过 config.json），
    /// 第一条断言即变红。
    #[test]
    fn test_context_window_three_levels() {
        let (dir, path) = write_config(
            "three_levels",
            r#"{
                "context_window": { "MiniMax-M3": 555000, "deepseek-chat": 64000 }
            }"#,
        );

        let mut config = test_config("MiniMax-M3");
        config.load_user_config(&path);
        assert_eq!(
            config.context_window(),
            555_000,
            "① config.json 覆盖应压过 ② 内置表"
        );

        config.model = "MiniMax-M2.7".into();
        assert_eq!(config.context_window(), 204_800, "② 内置表应压过 ③ 默认值");

        config.model = "unknown-model".into();
        assert_eq!(
            config.context_window(),
            128_000,
            "③ 未知 model 回退 RunLimits 默认"
        );

        // 同一份文件里 deepseek-chat 也被覆盖（证明是**按 model 查表**，
        // 不是「文件里存在任何覆盖就全局生效」）
        config.model = "deepseek-chat".into();
        assert_eq!(config.context_window(), 64_000);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件不存在 → 全默认，且**不 panic**（也不阻断启动）。
    #[test]
    fn test_context_window_missing_file_is_all_default() {
        let dir = temp_dir("missing_file");
        let path = dir.join("no_such_config.json");
        assert!(!path.exists());

        let mut config = test_config("deepseek-chat");
        config.load_user_config(&path);

        assert_eq!(config.context_window(), 128_000);
        assert_eq!(
            config.compact_policy(),
            ys_component::CompactPolicy::default()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **畸形 JSON 不 panic**：整份忽略，调用前状态原样保留。
    ///
    /// 变异：把 `load_user_config` 里的 `Err` 臂改成 `.unwrap()` → panic，本测试变红。
    #[test]
    fn test_malformed_json_is_ignored_without_panic() {
        let (dir, path) = write_config("malformed", "{ this is not json,,, }");

        let mut config = test_config("MiniMax-M3");
        config.load_user_config(&path);

        // 整份回退：内置表仍然生效（说明只是「用户覆盖没读到」，不是崩掉）
        assert_eq!(config.context_window(), 1_000_000);
        assert_eq!(config.compact_policy().trigger_ratio, 0.85);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空文件 / 顶层是数组（形状不对）同样只落日志、不 panic。
    #[test]
    fn test_wrong_shaped_json_is_ignored() {
        for (name, content) in [
            ("empty", ""),
            ("array", "[1, 2, 3]"),
            ("scalar", "42"),
            ("null", "null"),
            // context_window 是字符串而非对象
            ("bad_section", r#"{"context_window": "128000"}"#),
        ] {
            let (dir, path) = write_config(name, content);
            let mut config = test_config("MiniMax-M3");
            config.load_user_config(&path);
            assert_eq!(config.context_window(), 1_000_000, "case {name} 应整份回退");
            assert_eq!(
                config.compact_policy(),
                ys_component::CompactPolicy::default(),
                "case {name}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// **值非法被忽略**，且只丢非法那一条 —— 同文件里的合法条目照常生效。
    #[test]
    fn test_invalid_entries_are_dropped_individually() {
        let (dir, path) = write_config(
            "invalid_values",
            r#"{
                "context_window": {
                    "good-model": 200000,
                    "negative": -5,
                    "zero": 0,
                    "string": "128000",
                    "float": 1.5,
                    "bool": true,
                    "null": null,
                    "object": {}
                }
            }"#,
        );

        let mut config = test_config("good-model");
        config.load_user_config(&path);

        assert_eq!(config.context_window(), 200_000, "合法条目必须留下");
        for bad in [
            "negative", "zero", "string", "float", "bool", "null", "object",
        ] {
            config.model = bad.into();
            assert_eq!(
                config.context_window(),
                128_000,
                "非法条目 {bad} 应被丢弃（回退默认），而非生效"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `compaction` 只给 `trigger_ratio` 时，`keep_ratio` 仍为默认 0.10
    /// （两个比例**逐字段独立**覆盖）。
    #[test]
    fn test_compaction_partial_override_keeps_other_defaults() {
        let (dir, path) = write_config(
            "compaction_partial",
            r#"{"compaction": {"trigger_ratio": 0.70}}"#,
        );

        let mut config = test_config("deepseek-chat");
        config.load_user_config(&path);
        let policy = config.compact_policy();

        assert_eq!(policy.trigger_ratio, 0.70);
        assert_eq!(policy.keep_ratio, 0.10, "未给的比例必须保持默认");

        // 再补一份只给 keep_ratio 的，验证对称方向
        let (dir2, path2) = write_config(
            "compaction_partial_keep",
            r#"{"compaction": {"keep_ratio": 0.05}}"#,
        );
        let mut config2 = test_config("deepseek-chat");
        config2.load_user_config(&path2);
        let policy2 = config2.compact_policy();
        assert_eq!(policy2.trigger_ratio, 0.85, "未给的比例必须保持默认");
        assert_eq!(policy2.keep_ratio, 0.05);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// 比例越界 / 类型不对 → 落日志后忽略，回退默认值（不得 panic、不得生效）。
    #[test]
    fn test_invalid_ratios_are_ignored() {
        for (name, content) in [
            ("ratio_zero", r#"{"compaction": {"trigger_ratio": 0}}"#),
            (
                "ratio_negative",
                r#"{"compaction": {"trigger_ratio": -0.5}}"#,
            ),
            ("ratio_above_one", r#"{"compaction": {"keep_ratio": 1.5}}"#),
            ("ratio_string", r#"{"compaction": {"keep_ratio": "0.5"}}"#),
            ("ratio_null", r#"{"compaction": {"keep_ratio": null}}"#),
        ] {
            let (dir, path) = write_config(name, content);
            let mut config = test_config("deepseek-chat");
            config.load_user_config(&path);
            assert_eq!(
                config.compact_policy(),
                ys_component::CompactPolicy::default(),
                "case {name} 应回退默认策略"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// 边界合法值：`trigger_ratio = 1.0`（用满窗口才压缩）是合法意图，不得被丢弃。
    #[test]
    fn test_ratio_one_is_accepted() {
        let (dir, path) = write_config("ratio_one", r#"{"compaction": {"trigger_ratio": 1.0}}"#);
        let mut config = test_config("deepseek-chat");
        config.load_user_config(&path);
        assert_eq!(config.compact_policy().trigger_ratio, 1.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 未知字段被忽略（向后兼容：将来加字段不破坏旧 app）。
    #[test]
    fn test_unknown_fields_are_ignored() {
        let (dir, path) = write_config(
            "unknown_fields",
            r#"{
                "context_window": {"deepseek-chat": 64000},
                "future_section": {"whatever": 1},
                "top_level": 42
            }"#,
        );
        let mut config = test_config("deepseek-chat");
        config.load_user_config(&path);
        assert_eq!(config.context_window(), 64_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 重载语义：第二次 `load_user_config` **替换**（而非叠加）上一次的覆盖。
    #[test]
    fn test_reload_replaces_previous_overrides() {
        let (dir_a, path_a) = write_config(
            "reload_a",
            r#"{"context_window": {"deepseek-chat": 64000}}"#,
        );
        let (dir_b, path_b) = write_config("reload_b", r#"{"context_window": {}}"#);

        let mut config = test_config("deepseek-chat");
        config.load_user_config(&path_a);
        assert_eq!(config.context_window(), 64_000);

        config.load_user_config(&path_b);
        assert_eq!(
            config.context_window(),
            128_000,
            "第二次加载必须整体替换，旧覆盖不得残留"
        );

        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }

    /// 默认路径是 `~/.yushan/config.json`（**不是** `state.json` —— 那是 app 会覆盖的文件）。
    ///
    /// 本测试只读 `HOME`，但别处的测试会在 `env_lock` 下改写 / 移除它
    /// （`logging.rs`、`prompt.rs`、`main.rs`），故这里也必须持锁才能读到稳定值。
    #[test]
    fn test_default_user_config_path_shape() {
        let _guard = env_lock();
        let path = Config::default_user_config_path().expect("测试环境应有 HOME");
        assert!(path.ends_with(".yushan/config.json"), "{path:?}");
        assert!(!path.ends_with("state.json"));
        assert!(!path.ends_with("auth.json"));
    }

    /// 解析层与 `state.json` 的隔离：`load_user_config` 绝不写盘。
    #[test]
    fn test_load_user_config_does_not_write() {
        let (dir, path) = write_config(
            "readonly",
            r#"{"context_window": {"deepseek-chat": 64000}}"#,
        );
        let before = std::fs::read_to_string(&path).unwrap();
        let entries_before = std::fs::read_dir(&dir).unwrap().count();

        let mut config = test_config("deepseek-chat");
        config.load_user_config(&path);

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "文件不得被改写"
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            entries_before,
            "不得在目录里多出 state.json 之类的文件"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ───────────────────────── skills 装配（M4） ─────────────────────────

    /// 在 `root/<command>/SKILL.md` 落一份 skill。
    fn write_skill(root: &std::path::Path, command: &str, content: &str) {
        let dir = root.join(command);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    /// `from_env` 起步时**零 skill**（扫描是启动期显式的一步，不是构造副作用）；
    /// 空目录 / 不存在的根 ⇒ 零 skill、零诊断，且**其余字段不动**。
    ///
    /// 变异：把 `from_env` 改成顺带扫描真实 `~/.yushan/skills` ⇒ 第一组断言在开发机
    /// 上有 skill 时变红（且启动期会变成隐式 IO）。
    #[test]
    fn skills_start_empty_and_empty_roots_are_silent() {
        let _guard = env_lock();
        let _env = clear_api_env();

        let mut config = Config::from_env().unwrap();
        assert!(config.skills.is_empty(), "构造时不该有 skill");
        assert_eq!(config.skills_omitted, 0);

        let dir = temp_dir("skills_empty");
        let missing = dir.join("no_such_root");
        let model_before = config.model.clone();

        let diags = config.load_skills(&[missing, dir.clone()]);
        assert!(diags.is_empty(), "空根不产诊断：{diags:?}");
        assert!(config.skills.is_empty());
        assert_eq!(config.skills_omitted, 0);
        assert_eq!(config.model, model_before, "load_skills 不得碰别的字段");
        assert_eq!(config.cwd, std::env::current_dir().unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `load_skills` 按 roots 顺序取条目，**同名时 project 胜**（`roots[0]` = project）——
    /// 这是 [`crate::skills::scan`] 的 scope-by-position 契约在装配路径上的端到端钉法。
    ///
    /// ⚠️ 本用例**显式传 roots**（`[project, user]`），故它守的是 `scan` 的「位置即
    /// scope」契约，**不是** [`Config::default_skill_roots`] 的顺序 —— 反转后者**不会**
    /// 让本用例变红（它根本没调那个函数）。生产侧顺序的真正守卫是
    /// [`default_skill_roots_are_project_first_and_never_claude`]。
    ///
    /// 变异：把本用例里传的 roots 对调（user 在前）⇒ 本用例变红（胜者变成用户级正文）。
    #[test]
    fn load_skills_respects_root_order_project_wins() {
        let _guard = env_lock();
        let _env = clear_api_env();

        let project = temp_dir("skills_proj");
        let user = temp_dir("skills_user");
        write_skill(
            &project,
            "git-status",
            "---\nname: 项目版\ndescription: 项目级描述\n---\n项目正文\n",
        );
        write_skill(
            &user,
            "git-status",
            "---\nname: 用户版\ndescription: 用户级描述\n---\n用户正文\n",
        );

        let mut config = Config::from_env().unwrap();
        // **project 在前** —— 与 `default_skill_roots` 同序。
        let diags = config.load_skills(&[project.clone(), user.clone()]);

        assert_eq!(config.skills.len(), 1, "同名只留一条");
        assert_eq!(config.skills[0].description, "项目级描述");
        assert_eq!(config.skills[0].path, project.join("git-status/SKILL.md"));
        assert_eq!(diags.len(), 1, "遮蔽恰好一条诊断：{diags:?}");

        let _ = std::fs::remove_dir_all(&project);
        let _ = std::fs::remove_dir_all(&user);
    }

    /// **`default_skill_roots` 的顺序是语义**：第一条**必须**是 `<cwd>/.yushan/skills`
    /// （`scan` 靠位置判 project），第二条才是 `~/.yushan/skills`；且**不扫 `.claude`**。
    ///
    /// 变异：两条对调 ⇒ `roots[0]` 断言变红 —— 那会让用户级静默遮蔽项目级
    /// （与 ADR-0019 决策 6 相反），是**提示词缓存与同名遮蔽语义的真值**。
    #[test]
    fn default_skill_roots_are_project_first_and_never_claude() {
        let _guard = env_lock();
        let _env = EnvRestore::capture(&["HOME", "USERPROFILE"]);
        // SAFETY: 调用方持有 env_lock，独占这两个 key 的修改与恢复。
        unsafe {
            std::env::set_var("HOME", "/tmp/yushan-skills-home-probe");
            std::env::remove_var("USERPROFILE");
        }

        let cwd = std::path::Path::new("/work/proj");
        let roots = Config::default_skill_roots(cwd);
        assert_eq!(roots.len(), 2, "有 HOME 时两条根：{roots:?}");
        assert_eq!(
            roots[0],
            PathBuf::from("/work/proj/.yushan/skills"),
            "第一条必须是项目级（`scan` 靠位置判 project）"
        );
        assert_eq!(
            roots[1],
            PathBuf::from("/tmp/yushan-skills-home-probe/.yushan/skills"),
            "第二条才是用户级"
        );
        assert!(
            roots.iter().all(|r| !r.to_string_lossy().contains(".claude")),
            "不扫 `.claude/skills`（设计 D2）：{roots:?}"
        );

        // 无 HOME / USERPROFILE ⇒ 只剩项目那一条（不得凭空造出相对路径根）。
        // SAFETY: 同上，仍持锁。
        unsafe {
            std::env::remove_var("HOME");
        }
        assert_eq!(
            Config::default_skill_roots(cwd),
            vec![PathBuf::from("/work/proj/.yushan/skills")]
        );
    }

    /// **`cwd == $HOME` 时去重**：两条根是同一路径 ⇒ 只返回项目那一条。
    ///
    /// 不去重的话 `scan` 会把每个 skill 收两遍（一条 `Project`、一条 `User`，`path`
    /// 相同），`shadow_diagnostics` 便给每个 skill 产一条**假的**「project 级遮蔽
    /// user 级」诊断 —— 实测 56 条语料 ⇒ 56 行噪音，淹没 R4 依赖的真信号，启动期
    /// IO 还翻倍。
    ///
    /// 变异：去掉 `default_skill_roots` 里的 `if user != project` 判据 ⇒ 本用例
    /// 第一条断言变红（返回两条同路径的根）。
    #[test]
    fn default_skill_roots_dedup_when_cwd_is_home() {
        let _guard = env_lock();
        let home = "/tmp/yushan-skills-home-dedup";
        let _env = EnvRestore::capture(&["HOME", "USERPROFILE"]);
        // SAFETY: 调用方持有 env_lock，独占这两个 key 的修改与恢复。
        unsafe {
            std::env::set_var("HOME", home);
            std::env::remove_var("USERPROFILE");
        }

        // cwd == $HOME ⇒ 两条根同一路径，去重成一条。
        let roots = Config::default_skill_roots(std::path::Path::new(home));
        assert_eq!(
            roots,
            vec![PathBuf::from(format!("{home}/.yushan/skills"))],
            "cwd == HOME 时只应有一条根（去重）：{roots:?}"
        );

        // cwd != $HOME ⇒ 仍是两条，且 project 在前（顺序语义不变）。
        let roots = Config::default_skill_roots(std::path::Path::new("/work/proj"));
        assert_eq!(roots.len(), 2, "cwd != HOME 时两条根：{roots:?}");
        assert_eq!(
            roots[0],
            PathBuf::from("/work/proj/.yushan/skills"),
            "project 仍必须在第一条"
        );
        assert_eq!(roots[1], PathBuf::from(format!("{home}/.yushan/skills")));
    }
}
