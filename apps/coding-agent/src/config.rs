use std::path::PathBuf;

use agent_model::Model;

/// Model factory function type. Captures adapter-specific construction logic.
/// Returns None if config is incomplete (missing api_base/api_key).
type ModelFactory = Box<dyn Fn(&Config) -> Option<Box<dyn Model>> + Send + Sync>;

pub struct Config {
    pub api_base: Option<String>,
    pub api_key: Option<String>,
    pub model: String,
    pub cwd: PathBuf,
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
        let model = std::env::var("YUSHAN_MODEL")
            .unwrap_or_else(|_| "deepseek-chat".into());
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;

        Ok(Self {
            api_base,
            api_key,
            model,
            cwd,
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
}
