use std::path::PathBuf;

use ys_model::Model;

use crate::provider::ProviderRegistry;

/// Model factory 函数类型。封装适配器特有的构建逻辑。
/// config 不完整（缺 api_base/api_key）时返回 None。
type ModelFactory = Box<dyn Fn(&Config) -> Option<Box<dyn Model>> + Send + Sync>;

pub struct Config {
    pub api_base: Option<String>,
    pub api_key: Option<String>,
    pub model: String,
    pub cwd: PathBuf,
    pub provider: Option<String>,
    pub registry: ProviderRegistry,
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
            model_factory: None,
        })
    }

    pub fn is_configured(&self) -> bool {
        self.api_base.is_some() && self.api_key.is_some()
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // 清空任何已存在的环境变量
        unsafe {
            std::env::remove_var("YUSHAN_API_BASE");
            std::env::remove_var("YUSHAN_API_KEY");
            std::env::remove_var("OPENAI_API_BASE");
            std::env::remove_var("OPENAI_API_KEY");
        }

        let config = Config::from_env().unwrap();
        assert!(!config.is_configured());
        assert!(config.build_model().is_none());
        assert!(config.registry.find_provider("deepseek").is_some());
    }

    #[test]
    fn test_config_model_factory() {
        unsafe {
            std::env::remove_var("YUSHAN_API_BASE");
            std::env::remove_var("YUSHAN_API_KEY");
            std::env::remove_var("OPENAI_API_BASE");
            std::env::remove_var("OPENAI_API_KEY");
        }

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
        let mut config = Config::from_env().unwrap();
        config.provider = Some("deepseek".into());
        let compat = config.current_compat();
        assert!(compat.has_reasoning_content);
    }

    #[test]
    fn test_config_current_compat_unknown() {
        let config = Config::from_env().unwrap();
        // 未设置 provider -> "custom" -> standard
        let compat = config.current_compat();
        assert!(!compat.has_reasoning_content);
        assert!(!compat.tool_calls_as_text);
    }

    #[test]
    fn test_startup_recovery_from_auth() {
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
        unsafe {
            std::env::remove_var("YUSHAN_API_BASE");
            std::env::remove_var("YUSHAN_API_KEY");
            std::env::remove_var("OPENAI_API_BASE");
            std::env::remove_var("OPENAI_API_KEY");
        }

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
}
