use std::collections::HashMap;
use std::path::PathBuf;

use agent_model_openai_compatible::compat::ProviderCompat;
use serde::{Deserialize, Serialize};

/// 带元数据的已知 provider。
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    pub name: String,
    pub api_base: String,
    pub default_model: String,
    pub compat: ProviderCompat,
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

/// 从 API 拉取 models 的结果，区分错误类型。
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
    model_cache: HashMap<String, Vec<ApiModel>>,
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
            },
            ProviderInfo {
                name: "minimax".into(),
                api_base: "https://api.minimax.chat/v1".into(),
                default_model: "MiniMax-Text-01".into(),
                compat: ProviderCompat::minimax(),
            },
            ProviderInfo {
                name: "custom".into(),
                api_base: String::new(),
                default_model: String::new(),
                compat: ProviderCompat::standard(),
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

    /// 设置 auth 存储的覆盖路径（便于测试）。
    #[cfg(test)]
    pub(crate) fn set_auth_override(&mut self, path: PathBuf) {
        self.auth_override = Some(path);
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
            Err(e) => eprintln!("Warning: failed to parse auth.json: {e}"),
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

    /// 返回可用 models，优先用 cache，否则从 API 拉取。
    /// 出错时回退到静态已知 models。
    pub async fn available_models(&mut self, api_base: &str, api_key: &str) -> Vec<ApiModel> {
        if let Some(cached) = self.model_cache.get(api_base) {
            return cached.clone();
        }

        match Self::fetch_models(api_base, api_key).await {
            FetchModelsResult::Success(models) => {
                self.model_cache
                    .insert(api_base.to_string(), models.clone());
                models
            }
            FetchModelsResult::AuthError(_) | FetchModelsResult::NetworkError(_) => {
                Self::known_models_static()
            }
        }
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

        let mm = reg.compat_for("minimax");
        assert!(!mm.has_reasoning_content);
        assert!(mm.tool_calls_as_text);

        let custom = reg.compat_for("custom");
        assert!(!custom.has_reasoning_content);
        assert!(!custom.tool_calls_as_text);

        let unknown = reg.compat_for("nonexistent");
        assert!(!unknown.has_reasoning_content);
        assert!(!unknown.tool_calls_as_text);
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
}
