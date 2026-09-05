use std::path::PathBuf;

pub struct Config {
    pub api_base: Option<String>,
    pub api_key: Option<String>,
    pub model: String,
    pub cwd: PathBuf,
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
        })
    }

    pub fn is_configured(&self) -> bool {
        self.api_base.is_some() && self.api_key.is_some()
    }
}
