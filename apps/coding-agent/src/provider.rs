use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use ys_model_openai_compat::compat::ProviderCompat;

/// OpenCode Go 网关要求的会话头名。
///
/// 网关按该头做路由与 prompt cache 亲和；缺失或为空一律 `400 MissingSessionID`
/// （实测 `/chat/completions` 与 `/responses` 皆然）。
pub const OPENCODE_SESSION_HEADER: &str = "x-opencode-session";

/// 带元数据的已知 provider。
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    pub name: String,
    pub api_base: String,
    pub default_model: String,
    pub compat: ProviderCompat,
    /// 需要随**每个模型请求**发送的会话 id 头名（`None` = 无此要求）。
    ///
    /// 值是 [`session_id`] 给的**进程级稳定** id —— 网关只要求「一段对话用同一个
    /// id」，不要求它在进程之间延续。
    pub session_header: Option<String>,
}

/// auth.json 中按 provider 存储的凭证。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthEntry {
    pub api_base: String,
    pub api_key: String,
    pub model: String,
}

/// provider 的 /v1/models endpoint 返回的 model。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ApiModel {
    pub id: String,
    #[serde(default)]
    pub owned_by: Option<String>,
}

/// `/v1/models` 目录缓存：`api_base → 该端点的模型目录`。
///
/// 后台拉取任务产出、经信道送回 app 循环，由
/// [`ProviderRegistry::apply_model_cache`] 并入 [`ProviderRegistry::model_cache`]。
pub type ModelCache = HashMap<String, Vec<ApiModel>>;

/// 从 API 拉取 models 的结果，区分错误类型。
///
/// 生产消费者：[`ProviderRegistry::fetch_all`]（后台对每个已登录 provider 拉一次）。
/// 失败一律**静默** —— 不写缓存，`/model` 的候选回退静态表。
pub enum FetchModelsResult {
    Success(Vec<ApiModel>),
    /// 401 —— 提示用户重新登录
    AuthError(String),
    /// timeout/DNS —— 静默回退
    NetworkError(String),
}

/// GET /v1/models 的响应。
#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<ApiModel>,
}

/// provider 名称到凭证的映射。
type AuthStore = HashMap<String, AuthEntry>;

pub struct ProviderRegistry {
    providers: Vec<ProviderInfo>,
    auth_store: AuthStore,
    /// `/v1/models` 拉取结果缓存（按 api_base 键）。
    ///
    /// 由 [`ProviderRegistry::apply_model_cache`] 填充（后台拉取的结果从 app 循环
    /// 写回）；此后 `view::build_view` 每回合从它读候选模型 —— **渲染路径一次 HTTP
    /// 都不发**（设计 M4）。
    model_cache: ModelCache,
    /// auth 文件路径的覆盖项（测试用）。
    auth_override: Option<PathBuf>,
}

impl ProviderRegistry {
    /// 创建带内置 provider 的 registry。
    pub fn new() -> Self {
        let providers = vec![
            ProviderInfo {
                name: "deepseek".into(),
                api_base: "https://api.deepseek.com".into(),
                default_model: "deepseek-chat".into(),
                compat: ProviderCompat::deepseek(),
                session_header: None,
            },
            ProviderInfo {
                name: "minimax".into(),
                api_base: "https://api.minimax.chat/v1".into(),
                default_model: "MiniMax-Text-01".into(),
                compat: ProviderCompat::minimax(),
                session_header: None,
            },
            ProviderInfo {
                name: "opencode-go".into(),
                api_base: "https://opencode.ai/zen/go/v1".into(),
                default_model: "deepseek-flash".into(),
                compat: ProviderCompat::opencode_go(),
                session_header: Some(OPENCODE_SESSION_HEADER.into()),
            },
            ProviderInfo {
                name: "custom".into(),
                api_base: String::new(),
                default_model: String::new(),
                compat: ProviderCompat::standard(),
                session_header: None,
            },
        ];
        Self {
            providers,
            auth_store: HashMap::new(),
            model_cache: HashMap::new(),
            auth_override: None,
        }
    }

    /// 返回内置 provider 的引用。
    pub fn providers(&self) -> &[ProviderInfo] {
        &self.providers
    }

    /// 按名称查找 provider。
    pub fn find_provider(&self, name: &str) -> Option<&ProviderInfo> {
        self.providers.iter().find(|p| p.name == name)
    }

    /// 返回 provider 名称对应的 compat，未知时返回 standard()。
    pub fn compat_for(&self, name: &str) -> ProviderCompat {
        self.find_provider(name)
            .map(|p| p.compat.clone())
            .unwrap_or_else(ProviderCompat::standard)
    }

    /// 当前会话要随**每个模型请求**发送的额外头。
    ///
    /// 两条判定路径，前者优先：
    /// ① **按 provider 名** —— `/login` / `/model` 走的那条（`provider` 有值）；
    /// ② **按 api_base 与内置 provider 的 base 相等** —— 给 env 直连
    ///    （`YUSHAN_API_BASE` 指向同一网关）用：那条路径 `provider` **恒为 `None`**。
    ///
    /// 头值是**进程级稳定**的 [`session_id`]。同一进程内 `/new` / `/resume` 复用同一个
    /// id：网关侧的差别只是路由与 prompt cache 亲和，不影响请求正确性。
    ///
    /// 无要求的 provider 返回空 vec（绝大多数端点）。
    pub fn request_headers(
        &self,
        provider: Option<&str>,
        api_base: Option<&str>,
    ) -> Vec<(String, String)> {
        let header = provider
            .and_then(|name| self.find_provider(name))
            .and_then(|p| p.session_header.clone())
            .or_else(|| {
                let base = api_base?.trim_end_matches('/');
                self.providers
                    .iter()
                    .filter(|p| !p.api_base.is_empty())
                    .find(|p| p.api_base.trim_end_matches('/') == base)
                    .and_then(|p| p.session_header.clone())
            });
        match header {
            Some(name) => vec![(name, session_id())],
            None => Vec::new(),
        }
    }

    /// 设置 auth 存储的覆盖路径（便于测试）。
    #[cfg(test)]
    pub(crate) fn set_auth_override(&mut self, path: PathBuf) {
        self.auth_override = Some(path);
    }

    /// 测试用：直接塞一份「启动期拉到的目录」，免去起本地 HTTP 服务。
    #[cfg(test)]
    pub(crate) fn seed_cached_models(&mut self, base: &str, ids: &[&str]) {
        self.model_cache.insert(
            base.to_string(),
            ids.iter()
                .map(|id| ApiModel {
                    id: (*id).into(),
                    owned_by: None,
                })
                .collect(),
        );
    }

    /// 返回 auth.json 文件的路径。
    pub fn auth_path(&self) -> PathBuf {
        if let Some(ref p) = self.auth_override {
            return p.clone();
        }
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".yushan").join("auth.json")
    }

    /// 从磁盘读取 auth.json。文件不存在时什么都不做。
    pub fn load_auth(&mut self) {
        let path = self.auth_path();
        let data = match std::fs::read_to_string(&path) {
            Ok(d) => d,
            Err(_) => return,
        };
        match serde_json::from_str::<AuthStore>(&data) {
            Ok(store) => self.auth_store = store,
            // 库内部诊断 → 日志文件（设计 §5）：启动期 TUI 可能已起屏，
            // stderr 会冲掉整屏，而这里没有「历史输出」可看。
            Err(e) => crate::logging::log(&format!("failed to parse auth.json: {e}")),
        }
    }

    /// 插入 provider 的凭证并持久化到磁盘。
    pub fn save_auth(&mut self, provider_name: &str, entry: &AuthEntry) -> Result<(), String> {
        self.auth_store
            .insert(provider_name.to_string(), entry.clone());
        self.write_auth()
    }

    /// 移除 provider 的凭证并持久化到磁盘。
    pub fn remove_auth(&mut self, provider_name: &str) -> Result<(), String> {
        self.auth_store.remove(provider_name);
        self.write_auth()
    }

    /// 查找 provider 的凭证。
    pub fn auth_for(&self, provider_name: &str) -> Option<&AuthEntry> {
        self.auth_store.get(provider_name)
    }

    /// 存有凭证（已登录）的 provider 名称。
    /// 按注册顺序返回（deepseek、minimax、custom）。
    pub fn logged_in_providers(&self) -> Vec<String> {
        self.providers
            .iter()
            .filter(|p| self.auth_store.contains_key(&p.name))
            .map(|p| p.name.clone())
            .collect()
    }

    fn write_auth(&self) -> Result<(), String> {
        let path = self.auth_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
        }
        let json = serde_json::to_string_pretty(&self.auth_store)
            .map_err(|e| format!("serialize: {e}"))?;
        std::fs::write(&path, &json).map_err(|e| format!("write auth.json: {e}"))?;

        // 在 Unix 上设置受限权限。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&path, perms).map_err(|e| format!("set permissions: {e}"))?;
        }

        Ok(())
    }

    /// 从 provider 的 /v1/models endpoint 拉取 models。
    ///
    /// **只在后台拉取任务里调用**（[`ProviderRegistry::fetch_all`]，由 `main` 在 TUI
    /// 起屏**之后** `rt.spawn`）：拉到的目录缓存进 [`ProviderRegistry::model_cache`]，
    /// 由 [`ProviderRegistry::models_for_provider`] 读给 `/model` 浮层。**渲染路径
    /// 一次 HTTP 都不发** —— 故写入走 [`ProviderRegistry::apply_model_cache`]，
    /// 拉取本身绝不在 `build_view` 的调用栈上。
    pub async fn fetch_models(api_base: &str, api_key: &str) -> FetchModelsResult {
        let url = build_models_url(api_base);
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(e) => return FetchModelsResult::NetworkError(e.to_string()),
        };

        let resp = match client
            .get(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return FetchModelsResult::NetworkError(e.to_string()),
        };

        if resp.status().as_u16() == 401 {
            return FetchModelsResult::AuthError("API key rejected (401)".into());
        }
        if !resp.status().is_success() {
            return FetchModelsResult::NetworkError(format!("HTTP {}", resp.status()));
        }

        match resp.json::<ModelsResponse>().await {
            Ok(body) => FetchModelsResult::Success(body.data),
            Err(e) => FetchModelsResult::NetworkError(format!("parse error: {e}")),
        }
    }

    /// 已登录 provider 的 `(api_base, api_key)` 列表 —— 后台拉取 `/v1/models` 的输入。
    ///
    /// 只取 `api_base` 非空的条目（空 base 无处可拉，如未配 URL 的 `custom`）。
    /// 输入在**起屏前**从 registry 摘出，交给后台任务 —— 任务因此不借用 registry，
    /// 与 app 循环的写（[`Self::apply_model_cache`]）不共享任何状态。
    pub fn fetch_targets(&self) -> Vec<(String, String)> {
        self.providers
            .iter()
            .filter_map(|p| {
                self.auth_store
                    .get(&p.name)
                    .filter(|e| !e.api_base.is_empty())
                    .map(|e| (e.api_base.clone(), e.api_key.clone()))
            })
            .collect()
    }

    /// 后台：对**每个已登录 provider** 拉一次 `/v1/models`，返回 `api_base → 目录`
    /// （成功且非空才收进结果；供 [`Self::apply_model_cache`] 并入缓存）。
    ///
    /// **纯网络部分** —— 不触碰 `self`，故可在 TUI 起屏**之后** `rt.spawn` 到后台
    /// 任务里跑：起屏不再等网络，开屏即用（`/model` 候选先取静态表，目录到手后由
    /// app 循环刷新视图）。各 provider **并发**拉（`JoinSet`），故总耗时 ≈ 单次超时
    /// （5s），不随 provider 数线性增长。
    ///
    /// **失败一律静默**（超时 / DNS / 401 → 不写缓存，仅落日志）：
    /// [`Self::models_for_provider`] 会因此回退静态表 —— 网络问题绝不该让 `/model`
    /// 的候选变空，更不该让启动失败。
    pub async fn fetch_all(targets: Vec<(String, String)>) -> ModelCache {
        let mut cache: ModelCache = HashMap::new();
        let mut set = tokio::task::JoinSet::new();
        for (base, key) in targets {
            set.spawn(async move {
                let result = Self::fetch_models(&base, &key).await;
                (base, result)
            });
        }

        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((base, FetchModelsResult::Success(models))) => {
                    // 空目录视同失败：不写缓存 ⇒ `models_for_provider` 回退静态表
                    // （写进去反而会让候选整体变空）。
                    if !models.is_empty() {
                        cache.insert(base, models);
                    }
                }
                // 对用户静默（`/model` 照常有静态回退），但**落日志** —— 否则
                // 「候选怎么只有静态那几条」会成为一个无从排查的现象。
                Ok((base, FetchModelsResult::AuthError(e))) => {
                    crate::logging::log(&format!("model refresh {base}: auth error: {e}"));
                }
                Ok((base, FetchModelsResult::NetworkError(e))) => {
                    crate::logging::log(&format!("model refresh {base}: network error: {e}"));
                }
                Err(e) => crate::logging::log(&format!("model refresh task failed: {e}")),
            }
        }
        cache
    }

    /// 把后台拉取到的目录并入缓存（键为 api_base）。
    ///
    /// 唯一生产调用点：`app_loop::run` —— 后台任务的结果经信道送回 app 循环，由它
    /// 写入。registry 的**唯一写者仍是 app 循环**（与 `build_view` 的读同线程）。
    pub fn apply_model_cache(&mut self, cache: ModelCache) {
        self.model_cache.extend(cache);
    }

    /// 该 provider 的候选模型 id：优先后台拉到的目录，缺失则回退静态表中
    /// **属于它的**条目（[`Self::known_models_static`] 的 `owned_by` 即 provider 名）。
    ///
    /// 唯一消费者：`view::build_view`（`/model` 浮层的候选）。
    pub fn models_for_provider(&self, name: &str) -> Vec<String> {
        let base = self
            .auth_store
            .get(name)
            .map(|e| e.api_base.as_str())
            .or_else(|| self.find_provider(name).map(|p| p.api_base.as_str()));
        if let Some(models) = base.and_then(|base| self.model_cache.get(base)) {
            let ids: Vec<String> = models.iter().map(|m| m.id.clone()).collect();
            if !ids.is_empty() {
                return ids;
            }
        }
        Self::known_models_static()
            .into_iter()
            .filter(|m| m.owned_by.as_deref() == Some(name))
            .map(|m| m.id)
            .collect()
    }

    /// 哪个 provider 提供该 model —— 跨 provider 选模型时据此切端点
    /// （`capabilities::set_model`）。
    ///
    /// 先查启动期拉到的目录（api_base → provider 反查），再回退静态表的 `owned_by`。
    /// 都查不到返回 `None`（调用方**保持当前 provider 不变** —— 手敲的自定义模型名
    /// 不该把端点带跑）。
    pub fn provider_for_model(&self, model: &str) -> Option<String> {
        for (base, models) in &self.model_cache {
            if models.iter().any(|m| m.id == model)
                && let Some(p) = self.providers.iter().find(|p| {
                    self.auth_store
                        .get(&p.name)
                        .is_some_and(|e| &e.api_base == base)
                })
            {
                return Some(p.name.clone());
            }
        }
        Self::known_models_static()
            .into_iter()
            .find(|m| m.id == model)
            .and_then(|m| m.owned_by)
    }

    /// API 不可达时硬编码的回退 models。
    pub fn known_models_static() -> Vec<ApiModel> {
        vec![
            ApiModel {
                id: "deepseek-chat".into(),
                owned_by: Some("deepseek".into()),
            },
            ApiModel {
                id: "deepseek-reasoner".into(),
                owned_by: Some("deepseek".into()),
            },
            ApiModel {
                id: "MiniMax-Text-01".into(),
                owned_by: Some("minimax".into()),
            },
        ]
    }
}

/// 内置的 model → context window（token）表。
///
/// **解析键是 `model`，不是 `provider`**：`ProviderInfo` 每个 provider 只有一个
/// `default_model`，而同一 provider 下可以切多个模型（DeepSeek 至少
/// `deepseek-chat` / `deepseek-reasoner`）；且窗口本就是**模型**的属性。
///
/// 来源：厂商公开的模型规格。**值会过时** —— 这正是必须保留
/// `~/.yushan/config.json` 覆盖入口的原因（`Config::context_window` ② 级）。
///
/// 未收录的模型（含刻意留空的 MiniMax-Text-01，见下）一律返回 `None`，
/// 由调用方回退默认值。
///
/// | model | window | 依据 |
/// |---|---|---|
/// | `deepseek-chat` | 128K | DeepSeek 官方 API 文档（V3.1 起两个模型同为 128K） |
/// | `deepseek-reasoner` | 128K | 同上 |
/// | `MiniMax-M3` | 1M | MiniMax 官方 API 文档模型表 |
/// | `MiniMax-M2.7` | 204,800 | 同上 |
///
/// **刻意未收录**：`MiniMax-Text-01`（本仓库 minimax provider 的 default_model）。
/// 官方口径是「训练 1M / 推理可达 4M」，两条数字差 4 倍，**API 实际放行哪个无法确证**，
/// 故不填 —— 宁可回退 128_000（偏保守：提前压缩），也不编造一个可能让上下文溢出的值。
/// 用该模型的用户请写 `~/.yushan/config.json` 覆盖。
///
/// 另外收录 M3 / M2.7 还有一个工程理由：DeepSeek 两个模型都是 128K，
/// 恰好等于兜底默认值，若表里全是 128K，「② 内置表」这一级就**测不出来**。
const KNOWN_MODEL_WINDOWS: &[(&str, usize)] = &[
    ("deepseek-chat", 128_000),
    ("deepseek-reasoner", 128_000),
    ("MiniMax-M3", 1_000_000),
    ("MiniMax-M2.7", 204_800),
];

/// 查内置表。未知 model 返回 `None`（调用方回退默认值）。
///
/// 生产调用点：`Config::context_window` 的 ② 级（`config.rs`）。
pub fn builtin_context_window(model: &str) -> Option<usize> {
    KNOWN_MODEL_WINDOWS
        .iter()
        .find(|(name, _)| *name == model)
        .map(|(_, window)| *window)
}

/// 网关侧的会话 id：`ys-<pid>-<启动秒>_<纳秒>`，**每进程生成一次**。
///
/// 为什么是进程级而不是会话级：模型的构造点（`Config::build_model`）拿不到
/// `Wiring` 里的会话 id —— 启动期 model 先于 wiring 构造。网关对该值的要求只是
/// 「一段对话内稳定」，故进程内固定即可；进程 id + 启动时刻保证不同进程不撞。
pub fn session_id() -> String {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let d = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        format!(
            "ys-{}-{}_{:09}",
            std::process::id(),
            d.as_secs(),
            d.subsec_nanos()
        )
    })
    .clone()
}

/// 构建 /v1/models endpoint 的 URL。
/// 若 base 已以 "/v1" 或 "/v1/" 结尾，则追加 "/models"。
/// 否则追加 "/v1/models"。
pub fn build_models_url(api_base: &str) -> String {
    let base = api_base.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("yushan_test_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_auth_roundtrip() {
        let dir = temp_dir("auth_roundtrip");
        let auth_path = dir.join("auth.json");

        let mut reg = ProviderRegistry::new();
        reg.set_auth_override(auth_path.clone());

        let entry = AuthEntry {
            api_base: "https://api.deepseek.com".into(),
            api_key: "sk-test-key".into(),
            model: "deepseek-chat".into(),
        };
        reg.save_auth("deepseek", &entry).unwrap();

        // 创建新 registry 并从磁盘加载
        let mut reg2 = ProviderRegistry::new();
        reg2.set_auth_override(auth_path);
        reg2.load_auth();

        assert_eq!(reg2.auth_for("deepseek"), Some(&entry));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_auth_remove() {
        let dir = temp_dir("auth_remove");
        let auth_path = dir.join("auth.json");

        let mut reg = ProviderRegistry::new();
        reg.set_auth_override(auth_path.clone());

        let entry1 = AuthEntry {
            api_base: "https://api.deepseek.com".into(),
            api_key: "sk-ds".into(),
            model: "deepseek-chat".into(),
        };
        let entry2 = AuthEntry {
            api_base: "https://api.minimax.chat/v1".into(),
            api_key: "sk-mm".into(),
            model: "MiniMax-Text-01".into(),
        };

        reg.save_auth("deepseek", &entry1).unwrap();
        reg.save_auth("minimax", &entry2).unwrap();
        reg.remove_auth("deepseek").unwrap();

        // 重新加载并验证
        let mut reg2 = ProviderRegistry::new();
        reg2.set_auth_override(auth_path);
        reg2.load_auth();

        assert_eq!(reg2.auth_for("deepseek"), None);
        assert_eq!(reg2.auth_for("minimax"), Some(&entry2));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_compat_mapping() {
        let reg = ProviderRegistry::new();

        let ds = reg.compat_for("deepseek");
        assert!(ds.has_reasoning_content);
        assert!(!ds.tool_calls_as_text);
        assert!(ds.supports_stream_usage);

        let mm = reg.compat_for("minimax");
        assert!(!mm.has_reasoning_content);
        assert!(mm.tool_calls_as_text);
        assert!(!mm.supports_stream_usage);

        let custom = reg.compat_for("custom");
        assert!(!custom.has_reasoning_content);
        assert!(!custom.tool_calls_as_text);
        // standard() 默认开启流式 usage：env-var 配置落到 custom，
        // 若默认关闭则 token 统计恒为 0（回归点）。
        assert!(custom.supports_stream_usage);

        // 未知 provider 同样回退到 standard() → 默认开启。
        let unknown = reg.compat_for("nonexistent");
        assert!(!unknown.has_reasoning_content);
        assert!(!unknown.tool_calls_as_text);
        assert!(unknown.supports_stream_usage);
    }

    #[test]
    fn test_fetch_models_url() {
        assert_eq!(
            build_models_url("https://api.deepseek.com"),
            "https://api.deepseek.com/v1/models"
        );
        assert_eq!(
            build_models_url("https://api.deepseek.com/v1"),
            "https://api.deepseek.com/v1/models"
        );
        assert_eq!(
            build_models_url("https://api.deepseek.com/v1/"),
            "https://api.deepseek.com/v1/models"
        );
        assert_eq!(
            build_models_url("https://api.deepseek.com/"),
            "https://api.deepseek.com/v1/models"
        );
        assert_eq!(
            build_models_url("https://custom.example.com/api"),
            "https://custom.example.com/api/v1/models"
        );
    }

    #[test]
    fn test_known_models_fallback() {
        let models = ProviderRegistry::known_models_static();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "deepseek-chat");
        assert_eq!(models[0].owned_by.as_deref(), Some("deepseek"));
        assert_eq!(models[1].id, "deepseek-reasoner");
        assert_eq!(models[1].owned_by.as_deref(), Some("deepseek"));
        assert_eq!(models[2].id, "MiniMax-Text-01");
        assert_eq!(models[2].owned_by.as_deref(), Some("minimax"));
    }

    /// opencode-go 的内置元数据：网关 base、默认模型（**必须是 chat/completions
    /// 协议的模型** —— 适配器只会打那个端点）与会话头。
    #[test]
    fn test_opencode_go_provider_metadata() {
        let reg = ProviderRegistry::new();
        let provider = reg.find_provider("opencode-go").expect("已注册");
        assert_eq!(provider.api_base, "https://opencode.ai/zen/go/v1");
        assert_eq!(provider.default_model, "deepseek-flash");
        assert_eq!(
            provider.session_header.as_deref(),
            Some(OPENCODE_SESSION_HEADER)
        );
        // 路由与 prompt cache 亲和都靠 `reasoning_content` 方言解析思考增量。
        assert!(reg.compat_for("opencode-go").has_reasoning_content);
    }

    /// 按 provider 名给出会话头（`/login` / `/model` 那条路径）。
    #[test]
    fn test_request_headers_by_provider_name() {
        let reg = ProviderRegistry::new();
        let headers = reg.request_headers(Some("opencode-go"), None);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, OPENCODE_SESSION_HEADER);
        assert!(!headers[0].1.is_empty(), "网关拒绝空会话 id");

        // 其余内置 provider 无此要求。
        assert!(reg.request_headers(Some("deepseek"), None).is_empty());
        assert!(reg.request_headers(Some("custom"), None).is_empty());
        assert!(reg.request_headers(None, None).is_empty());
    }

    /// env 直连（`YUSHAN_API_BASE` 指向网关）时 `provider` 恒为 `None`：
    /// 会话头必须能**只凭 api_base** 得出，否则那条路径每次请求都 400。
    #[test]
    fn test_request_headers_by_api_base_for_env_path() {
        let reg = ProviderRegistry::new();
        let headers = reg.request_headers(None, Some("https://opencode.ai/zen/go/v1"));
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, OPENCODE_SESSION_HEADER);

        // 尾斜杠不影响匹配。
        assert_eq!(
            reg.request_headers(None, Some("https://opencode.ai/zen/go/v1/"))
                .len(),
            1
        );
        // 别的 base 一律不加头。
        assert!(
            reg.request_headers(None, Some("https://api.deepseek.com"))
                .is_empty()
        );
        assert!(
            reg.request_headers(None, Some("https://example.com/v1"))
                .is_empty()
        );
    }

    /// 会话 id 在进程内**稳定**：同一进程两次取到的值必须相同，
    /// 否则网关侧每次请求都是新会话（丢掉路由与缓存亲和）。
    #[test]
    fn test_session_id_is_process_stable() {
        assert_eq!(session_id(), session_id());
        assert!(session_id().starts_with("ys-"));
    }

    #[test]
    fn test_builtin_context_window_known() {
        assert_eq!(builtin_context_window("deepseek-chat"), Some(128_000));
        assert_eq!(builtin_context_window("deepseek-reasoner"), Some(128_000));
        assert_eq!(builtin_context_window("MiniMax-M3"), Some(1_000_000));
        assert_eq!(builtin_context_window("MiniMax-M2.7"), Some(204_800));
    }

    /// 未知 model → `None`（由调用方回退默认值）。
    ///
    /// 明列 `MiniMax-Text-01`：它是本仓库 minimax provider 的 default_model，
    /// 但官方「训练 1M / 推理 4M」两条口径矛盾，**刻意不收录**（见表注释）。
    #[test]
    fn test_builtin_context_window_unknown_is_none() {
        assert_eq!(builtin_context_window("MiniMax-Text-01"), None);
        assert_eq!(builtin_context_window("my-1m-model"), None);
        assert_eq!(builtin_context_window(""), None);
        // 精确匹配：不做大小写折叠 / 前缀匹配
        assert_eq!(builtin_context_window("DeepSeek-Chat"), None);
        assert_eq!(builtin_context_window("deepseek-chat-v3"), None);
    }

    #[test]
    fn test_auth_load_nonexistent() {
        let dir = temp_dir("auth_nonexistent");
        let auth_path = dir.join("nonexistent_auth.json");

        let mut reg = ProviderRegistry::new();
        reg.set_auth_override(auth_path);
        reg.load_auth(); // 不应 panic 或报错

        assert!(reg.auth_for("deepseek").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_save_auth_sets_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("auth_permissions");
        let auth_path = dir.join("auth.json");

        let mut reg = ProviderRegistry::new();
        reg.set_auth_override(auth_path.clone());

        let entry = AuthEntry {
            api_base: "https://api.deepseek.com".into(),
            api_key: "sk-test-perms".into(),
            model: "deepseek-chat".into(),
        };
        reg.save_auth("deepseek", &entry).unwrap();

        let metadata = std::fs::metadata(&auth_path).unwrap();
        let mode = metadata.permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "auth.json should have 0600 permissions, got {:#o}",
            mode & 0o777
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `models_for_provider`：**优先启动期拉到的目录**，缺失才回退静态表 ——
    /// 且回退只给**该 provider 的**条目（不再把 deepseek 的名字塞给 minimax）。
    ///
    /// 变异：把 cache 分支删掉 → 第二条断言变红（拿到的仍是静态的 deepseek-chat）。
    #[test]
    fn test_models_for_provider_prefers_fetched_catalog_over_static() {
        let mut reg = ProviderRegistry::new();
        reg.set_auth_override(PathBuf::from("/nonexistent-auth.json"));

        // 未拉取 → 静态表中属于 deepseek 的那两条；minimax 的名字不得混进来。
        assert_eq!(
            reg.models_for_provider("deepseek"),
            vec!["deepseek-chat".to_string(), "deepseek-reasoner".to_string()]
        );
        assert!(
            !reg.models_for_provider("deepseek")
                .contains(&"MiniMax-Text-01".to_string()),
            "静态回退必须按 owned_by 过滤，不得跨 provider"
        );

        // 拉到了目录 → 用目录（静态表不再参与）
        reg.seed_cached_models("https://api.deepseek.com", &["deepseek-v4"]);
        assert_eq!(
            reg.models_for_provider("deepseek"),
            vec!["deepseek-v4".to_string()]
        );

        // 空目录视同没拉到 → 回退静态表（`fetch_all` 不写空目录，这里直接构造）
        reg.seed_cached_models("https://api.deepseek.com", &[]);
        assert_eq!(
            reg.models_for_provider("deepseek"),
            vec!["deepseek-chat".to_string(), "deepseek-reasoner".to_string()]
        );
    }

    /// `provider_for_model`：目录（经 auth 条目把 base 绑回 provider）优先，
    /// 回退静态表的 `owned_by`；两处都没有 → `None`（调用方保持当前 provider）。
    #[test]
    fn test_provider_for_model_resolves_catalog_then_static() {
        let mut reg = ProviderRegistry::new();
        reg.set_auth_override(PathBuf::from("/nonexistent-auth.json"));

        // 静态表兜底
        assert_eq!(
            reg.provider_for_model("deepseek-chat").as_deref(),
            Some("deepseek")
        );
        assert_eq!(
            reg.provider_for_model("MiniMax-Text-01").as_deref(),
            Some("minimax")
        );
        assert_eq!(reg.provider_for_model("gpt-4o"), None, "两处都没有 → None");

        // 目录优先：新模型名只存在于拉到的目录里
        reg.auth_store.insert(
            "minimax".into(),
            AuthEntry {
                api_base: "https://api.minimax.chat/v1".into(),
                api_key: "sk-mm".into(),
                model: "MiniMax-Text-01".into(),
            },
        );
        reg.seed_cached_models("https://api.minimax.chat/v1", &["MiniMax-M3"]);
        assert_eq!(
            reg.provider_for_model("MiniMax-M3").as_deref(),
            Some("minimax"),
            "目录里的新模型应能反查到 provider"
        );
    }
}
