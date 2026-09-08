use std::path::PathBuf;

use agent_model::Model;

use crate::provider::ProviderRegistry;

/// Model factory function type. Captures adapter-specific construction logic.
/// Returns None if config is incomplete (missing api_base/api_key).
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
    /// Load from environment variables. All fields except `cwd` are optional.
    /// The agent can start without API credentials; configure via `/login`.
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

    /// Set the model factory. Called once in main.rs after adapter types are known.
    pub fn set_model_factory(
        &mut self,
        factory: impl Fn(&Config) -> Option<Box<dyn Model>> + Send + Sync + 'static,
    ) {
        self.model_factory = Some(Box::new(factory));
    }

    /// Build a model from current config. Delegates to the factory.
    pub fn build_model(&self) -> Option<Box<dyn Model>> {
        self.model_factory.as_ref().and_then(|f| f(self))
    }

    /// Return ProviderCompat for the current provider (or "custom" if none set).
    pub fn current_compat(&self) -> agent_model_openai_compatible::compat::ProviderCompat {
        let name = self.provider.as_deref().unwrap_or("custom");
        self.registry.compat_for(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Dummy model for testing
    struct DummyModel;

    #[async_trait::async_trait]
    impl agent_model::Model for DummyModel {
        fn model_id(&self) -> &str {
            "dummy"
        }
        async fn complete(
            &self,
            _request: agent_model::ModelRequest,
            _sink: &mut dyn agent_model::ModelEventSink,
        ) -> Result<agent_model::ModelResponse, agent_model::ModelError> {
            unimplemented!("test dummy")
        }
    }

    #[test]
    fn test_config_from_env_no_vars() {
        // Clear any existing env vars
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

        // Set up config with credentials
        config.api_base = Some("https://api.example.com".into());
        config.api_key = Some("sk-test".into());

        // Without factory, still returns None
        assert!(config.build_model().is_none());

        // Register a factory
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
        // No provider set -> "custom" -> standard
        let compat = config.current_compat();
        assert!(!compat.has_reasoning_content);
        assert!(!compat.tool_calls_as_text);
    }

    #[test]
    fn test_startup_recovery_from_auth() {
        // Prepare a temp auth.json with deepseek credentials
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

        // Create Config with no env vars (not configured)
        unsafe {
            std::env::remove_var("YUSHAN_API_BASE");
            std::env::remove_var("YUSHAN_API_KEY");
            std::env::remove_var("OPENAI_API_BASE");
            std::env::remove_var("OPENAI_API_KEY");
        }

        let mut config = Config::from_env().unwrap();
        config.registry.set_auth_override(auth_path);
        config.registry.load_auth();

        // Simulate the main.rs recovery loop
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
