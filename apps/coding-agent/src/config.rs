use std::path::PathBuf;

pub struct Config {
    pub api_base: String,
    pub api_key: String,
    pub model: String,
    pub cwd: PathBuf,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let api_base = std::env::var("YUSHAN_API_BASE")
            .or_else(|_| std::env::var("OPENAI_API_BASE"))
            .map_err(|_| "Set YUSHAN_API_BASE or OPENAI_API_BASE")?;
        let api_key = std::env::var("YUSHAN_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .map_err(|_| "Set YUSHAN_API_KEY or OPENAI_API_KEY")?;
        let model = std::env::var("YUSHAN_MODEL").unwrap_or_else(|_| "deepseek-chat".into());
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;

        Ok(Self {
            api_base,
            api_key,
            model,
            cwd,
        })
    }
}
