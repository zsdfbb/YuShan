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
    /// 厂商给的**展示名**（可选）。
    #[serde(default)]
    pub name: Option<String>,
    /// 厂商声明的**上下文窗口**（token 总容量 = 输入 + 输出；可选）。
    ///
    /// 目录里报了就用它 —— 窗口的第一手来源（`Config::context_window_for` 的 ② 级）。
    /// 厂商不报（如 opencode-go 网关）才回退默认值。
    #[serde(default)]
    pub context_window: Option<usize>,
}

/// `/v1/models` 目录缓存：`api_base → 该端点的模型目录`。
///
/// 后台拉取任务产出、经信道送回 app 循环，由
/// [`ProviderRegistry::apply_model_cache`] 并入 [`ProviderRegistry::model_cache`]。
pub type ModelCache = HashMap<String, Vec<ApiModel>>;

/// 从 API 拉取 models 的结果，区分错误类型。
///
/// 生产消费者：[`ProviderRegistry::fetch_all`]（后台对每个已登录 provider 拉一次）、
/// `app_loop` 的 `/login`（登录时拉该 provider 一次）。
/// 失败一律**静默** —— 不写缓存，`/model` 候选就没有这一家的条目。
pub enum FetchModelsResult {
    Success(Vec<ApiModel>),
    /// 401 —— 凭证被拒
    AuthError(String),
    /// timeout / DNS / 非 2xx —— 端点没答上来
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
    ///
    /// 内置项只登记**端点与方言**（`api_base` / `compat` / 会话头）—— 模型名一律来自
    /// 该端点的 `/v1/models` 目录，**不在代码里写死**。
    pub fn new() -> Self {
        let providers = vec![
            ProviderInfo {
                name: "deepseek".into(),
                api_base: "https://api.deepseek.com".into(),
                compat: ProviderCompat::deepseek(),
                session_header: None,
            },
            ProviderInfo {
                name: "minimax".into(),
                api_base: "https://api.minimax.chat/v1".into(),
                compat: ProviderCompat::minimax(),
                session_header: None,
            },
            ProviderInfo {
                name: "opencode-go".into(),
                api_base: "https://opencode.ai/zen/go/v1".into(),
                compat: ProviderCompat::opencode_go(),
                session_header: Some(OPENCODE_SESSION_HEADER.into()),
            },
            ProviderInfo {
                name: "custom".into(),
                api_base: String::new(),
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
        self.seed_cached_catalog(
            base,
            &ids.iter()
                .map(|id| ApiModel {
                    id: (*id).into(),
                    owned_by: None,
                    name: None,
                    context_window: None,
                })
                .collect::<Vec<_>>(),
        );
    }

    /// 测试用：塞一份带窗口的目录（`context_window` 是 `/v1/models` 的第二手信息）。
    #[cfg(test)]
    pub(crate) fn seed_cached_catalog(&mut self, base: &str, models: &[ApiModel]) {
        self.model_cache
            .insert(cache_key(base).to_string(), models.to_vec());
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

    /// 需要拉 `/v1/models` 的 `(api_base, api_key)` 列表 —— 后台拉取的输入。
    ///
    /// = **每个已登录 provider**（`api_base` 非空；空 base 无处可拉，如未配 URL 的
    /// `custom`）+ **env 直连**（`env_direct` 给了且该 base 未被某个 auth 条目覆盖）。
    /// 后者是 `/model` 候选与窗口在 `YUSHAN_API_BASE` 那条路径上的**唯一来源** ——
    /// 那条路径 `provider` 恒为 `None`，registry 里没有它的名字。
    ///
    /// 输入在**起屏前**从 registry 摘出，交给后台任务 —— 任务因此不借用 registry，
    /// 与 app 循环的写（[`Self::apply_model_cache`]）不共享任何状态。
    pub fn fetch_targets(&self, env_direct: Option<(String, String)>) -> Vec<(String, String)> {
        let mut targets: Vec<(String, String)> = self
            .providers
            .iter()
            .filter_map(|p| {
                self.auth_store
                    .get(&p.name)
                    .filter(|e| !e.api_base.is_empty())
                    .map(|e| (e.api_base.clone(), e.api_key.clone()))
            })
            .collect();
        if let Some((base, key)) = env_direct.filter(|(b, _)| !b.is_empty())
            && !targets
                .iter()
                .any(|(b, _)| cache_key(b) == cache_key(&base))
        {
            targets.push((base, key));
        }
        targets
    }

    /// 后台：对**每个已登录 provider** 拉一次 `/v1/models`，返回 `api_base → 目录`
    /// （成功且非空才收进结果；供 [`Self::apply_model_cache`] 并入缓存）。
    ///
    /// **纯网络部分** —— 不触碰 `self`，故可在 TUI 起屏**之后** `rt.spawn` 到后台
    /// 任务里跑：起屏不再等网络（`/model` 候选在目录到手前只有当前那条模型，到手后
    /// 由 app 循环刷新视图）。各 provider **并发**拉（`JoinSet`），故总耗时 ≈ 单次
    /// 超时（5s），不随 provider 数线性增长。
    ///
    /// **失败一律静默**（超时 / DNS / 401 → 不写缓存，仅落日志）：那一家在 `/model`
    /// 里就没有候选（模型名只有目录一个来源）。网络问题不该让启动失败。
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
                    // 空目录视同失败：不写缓存（写进去会让候选整体变空，与"没拉到"
                    // 在行为上等价，但会多占一条无意义的记录）。
                    if !models.is_empty() {
                        cache.insert(cache_key(&base).to_string(), models);
                    }
                }
                // 对用户静默，但**落日志** —— 否则「这家怎么一个候选都没有」
                // 会成为一个无从排查的现象。
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

    /// 该 provider **拉到的**候选模型 id（未拉到 ⇒ 空）。
    ///
    /// 唯一消费者：`view::build_view`（`/model` 浮层的候选）。模型名只有这一个来源 ——
    /// 拉不到就是没有，**不做静态回退**（写死的名字必然过时，且会与端点真正提供的
    /// 模型对不上）。
    pub fn models_for_provider(&self, name: &str) -> Vec<String> {
        let base = self
            .auth_store
            .get(name)
            .map(|e| e.api_base.clone())
            .or_else(|| self.find_provider(name).map(|p| p.api_base.clone()));
        base.map(|base| self.models_for_base(&base))
            .unwrap_or_default()
    }

    /// 某个 `api_base` 的目录（env 直连没有 provider 名，按 base 取）。
    pub fn models_for_base(&self, api_base: &str) -> Vec<String> {
        self.model_cache
            .get(cache_key(api_base))
            .map(|models| models.iter().map(|m| m.id.clone()).collect())
            .unwrap_or_default()
    }

    /// 目录里该 model 的上下文窗口（token）。**窗口的第一手来源**
    /// （`Config::context_window_for` 的 ② 级）。
    ///
    /// 遍历所有已拉到的目录：同名 id 出现在两家的目录里时，各家报的窗口本就该一致。
    /// 目录里没有该 id、或该条目没报窗口 ⇒ `None`（调用方回退默认值）。
    pub fn catalog_context_window(&self, model: &str) -> Option<usize> {
        self.model_cache
            .values()
            .flatten()
            .find(|m| m.id == model)
            .and_then(|m| m.context_window)
    }

    /// 哪个 provider 提供该 model —— 跨 provider 选模型时据此切端点
    /// （`capabilities::set_model`）。
    ///
    /// 只认启动期拉到的目录（api_base → provider 反查）。查不到返回 `None`
    /// （调用方**保持当前 provider 不变** —— 手敲的自定义模型名不该把端点带跑；
    /// 要显式指定归属就写 `provider/model` 限定名）。
    pub fn provider_for_model(&self, model: &str) -> Option<String> {
        for (base, models) in &self.model_cache {
            if models.iter().any(|m| m.id == model)
                && let Some(p) = self.providers.iter().find(|p| {
                    self.auth_store
                        .get(&p.name)
                        .is_some_and(|e| cache_key(&e.api_base) == cache_key(base))
                })
            {
                return Some(p.name.clone());
            }
        }
        None
    }
}

/// 目录缓存的键：抹掉尾斜杠，令 `https://x/v1` 与 `https://x/v1/` 命中同一条目。
fn cache_key(api_base: &str) -> &str {
    api_base.trim_end_matches('/')
}

/// 内置的 model → context window（token）兜底表。
///
/// **只收录端点的 `/v1/models` 报不出窗口的模型**。窗口的第一手来源是目录
/// （[`ProviderRegistry::catalog_context_window`]，`Config::context_window_for` 的 ② 级）
/// —— 厂商自己在目录里报的值永远比这里新，故本表是 ③ 级：只在目录给不出时用。
///
/// | model | window | 依据 |
/// |---|---|---|
/// | `MiniMax-M3` | 1M | MiniMax 官方 API 文档模型表 |
/// | `MiniMax-M2.7` | 204,800 | 同上 |
///
/// **刻意未收录**：MiniMax 其余模型（如 `MiniMax-Text-01`）。官方口径是「训练 1M /
/// 推理可达 4M」，两条数字差 4 倍，**API 实际放行哪个无法确证** —— 宁可不填、回退
/// 兜底默认值，也不编造一个可能让上下文溢出的值。用这些模型的用户
/// 请写 `~/.yushan/config.json` 覆盖，或让端点自己在目录里报窗口。
///
/// 表中值**刻意不取兜底默认值**：若表里全是它，这一级就**测不出来**。
const KNOWN_MODEL_WINDOWS: &[(&str, usize)] =
    &[("MiniMax-M3", 1_000_000), ("MiniMax-M2.7", 204_800)];

/// 查兜底表。未知 model 返回 `None`（调用方回退默认值）。
///
/// 生产调用点：`Config::context_window_for` 的 ③ 级（`config.rs`）。
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

    /// opencode-go 的内置元数据：网关 base 与会话头（**模型名不在这里** —— 它只来自
    /// 网关的 `/v1/models`）。
    #[test]
    fn test_opencode_go_provider_metadata() {
        let reg = ProviderRegistry::new();
        let provider = reg.find_provider("opencode-go").expect("已注册");
        assert_eq!(provider.api_base, "https://opencode.ai/zen/go/v1");
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

    /// 兜底表只覆盖**目录报不出窗口**的模型（MiniMax 那几个）。DeepSeek 的窗口由
    /// 目录自己报（`catalog_context_window`），表里**不该**再有它的名字 ——
    /// 那正是「写死的名字必然过时」的现场。
    #[test]
    fn test_builtin_context_window_known() {
        assert_eq!(builtin_context_window("MiniMax-M3"), Some(1_000_000));
        assert_eq!(builtin_context_window("MiniMax-M2.7"), Some(204_800));
        assert_eq!(builtin_context_window("deepseek-chat"), None);
        assert_eq!(builtin_context_window("deepseek-flash"), None);
    }

    /// 未知 model → `None`（由调用方回退默认值）。
    ///
    /// 明列 `MiniMax-Text-01`：官方「训练 1M / 推理 4M」两条口径矛盾，**刻意不收录**
    /// （见表注释）—— 它到底多大只能由端点自己报，或由用户写 config.json。
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

    /// `models_for_provider`：**只**返回拉到的目录，未拉到就是空 —— 不再有静态回退。
    ///
    /// 变异：把 `models_for_base` 换成读静态表 ⇒ 第一条断言（未拉取 ⇒ 空）变红。
    #[test]
    fn test_models_for_provider_is_catalog_only() {
        let mut reg = ProviderRegistry::new();
        reg.set_auth_override(PathBuf::from("/nonexistent-auth.json"));

        // 未拉取 → 空。写死的名字不再充数（它们端点可能早就不认了）。
        assert!(reg.models_for_provider("deepseek").is_empty());
        assert!(reg.models_for_provider("minimax").is_empty());

        // 拉到了目录 → 按 auth 条目的 base 取回该 provider 的目录
        reg.seed_cached_models("https://api.deepseek.com", &["deepseek-flash"]);
        assert_eq!(
            reg.models_for_provider("deepseek"),
            vec!["deepseek-flash".to_string()]
        );
        // 目录不跨 provider 串味
        assert!(reg.models_for_provider("minimax").is_empty());

        // 尾斜杠不影响命中（缓存键抹尾斜杠）
        assert_eq!(
            reg.models_for_base("https://api.deepseek.com/"),
            vec!["deepseek-flash".to_string()]
        );
    }

    /// `provider_for_model`：只认目录（经 auth 条目把 base 绑回 provider）；查不到 →
    /// `None`（调用方保持当前 provider，不猜端点）。
    #[test]
    fn test_provider_for_model_resolves_from_catalog() {
        let mut reg = ProviderRegistry::new();
        reg.set_auth_override(PathBuf::from("/nonexistent-auth.json"));

        assert_eq!(
            reg.provider_for_model("deepseek-chat"),
            None,
            "没有目录 ⇒ None"
        );
        assert_eq!(reg.provider_for_model("gpt-4o"), None);

        reg.auth_store.insert(
            "minimax".into(),
            AuthEntry {
                api_base: "https://api.minimax.chat/v1".into(),
                api_key: "sk-mm".into(),
                model: "MiniMax-M3".into(),
            },
        );
        reg.seed_cached_models("https://api.minimax.chat/v1", &["MiniMax-M3"]);
        assert_eq!(
            reg.provider_for_model("MiniMax-M3").as_deref(),
            Some("minimax"),
            "目录里的模型应能反查到 provider"
        );
        // 尾斜杠经缓存键归一化，不影响反查
        reg.auth_store.get_mut("minimax").unwrap().api_base = "https://api.minimax.chat/v1/".into();
        assert_eq!(
            reg.provider_for_model("MiniMax-M3").as_deref(),
            Some("minimax")
        );
    }

    /// `catalog_context_window`：窗口取自目录里厂商自己报的值；没报 / 没这个 id ⇒ None。
    #[test]
    fn test_catalog_context_window() {
        let mut reg = ProviderRegistry::new();
        reg.seed_cached_catalog(
            "https://api.deepseek.com",
            &[
                ApiModel {
                    id: "deepseek-flash".into(),
                    owned_by: Some("deepseek".into()),
                    name: Some("DeepSeek-V4.1-Flash".into()),
                    context_window: Some(1_048_576),
                },
                ApiModel {
                    id: "no-window".into(),
                    owned_by: None,
                    name: None,
                    context_window: None,
                },
            ],
        );
        assert_eq!(
            reg.catalog_context_window("deepseek-flash"),
            Some(1_048_576)
        );
        assert_eq!(reg.catalog_context_window("no-window"), None);
        assert_eq!(reg.catalog_context_window("not-in-catalog"), None);
    }

    /// `fetch_targets`：已登录 provider + **env 直连**端点；env 的 base 已被某个
    /// auth 条目覆盖时**不重复**（同一端点只拉一次）。
    #[test]
    fn test_fetch_targets_includes_env_direct_without_duplicating() {
        let mut reg = ProviderRegistry::new();
        reg.set_auth_override(PathBuf::from("/nonexistent-auth.json"));
        assert!(reg.fetch_targets(None).is_empty());

        // env 直连：registry 里没有它的名字，也要拉 —— 否则那条路径的 `/model` 无候选。
        let env = Some(("https://api.deepseek.com".to_string(), "sk-env".to_string()));
        assert_eq!(
            reg.fetch_targets(env.clone()),
            vec![("https://api.deepseek.com".to_string(), "sk-env".to_string())]
        );

        // 同一 base 已有 auth 条目（尾斜杠视为同一个端点）⇒ 不再重复拉。
        reg.auth_store.insert(
            "deepseek".to_string(),
            AuthEntry {
                api_base: "https://api.deepseek.com/".into(),
                api_key: "sk-auth".into(),
                model: "deepseek-flash".into(),
            },
        );
        let targets = reg.fetch_targets(env);
        assert_eq!(targets.len(), 1, "{targets:?}");
        assert_eq!(targets[0].1, "sk-auth", "auth 条目的凭证优先");
    }
}
