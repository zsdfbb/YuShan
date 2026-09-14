//! 会话状态持久化（state.json）。
//!
//! state 独立于 auth.json 存放——不含敏感数据，但为与 `ProviderRegistry::write_auth`
//! 保持一致仍限制为 0o600。
//!
//! 持久化字段（可扩展）：
//! - `last_active_provider` — 最近一次登录的 provider 名称
//! - `last_active_model` — 最近一次选中的 model 名称

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct AppState {
    pub last_active_provider: Option<String>,
    pub last_active_model: Option<String>,
}

pub struct StateStore {
    path_override: Option<PathBuf>,
}

impl StateStore {
    pub fn new() -> Self {
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
        let path_override = home.map(|h| PathBuf::from(h).join(".yushan").join("state.json"));
        Self { path_override }
    }

    pub fn load(&self) -> AppState {
        let Some(path) = self.path() else {
            return AppState::default();
        };
        match std::fs::read_to_string(path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_else(|e| {
                // 库内部诊断 → 日志文件（设计 §5），与 auth.json 同理。
                crate::logging::log(&format!("failed to parse state.json: {e}"));
                AppState::default()
            }),
            Err(_) => AppState::default(),
        }
    }

    pub fn save(&self, state: &AppState) -> Result<(), String> {
        let Some(path) = self.path() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
        }
        let json = serde_json::to_string_pretty(state).map_err(|e| format!("serialize: {e}"))?;
        std::fs::write(path, &json).map_err(|e| format!("write state.json: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("set permissions: {e}"))?;
        }
        Ok(())
    }

    pub fn exists(&self) -> bool {
        self.path().map(|p| p.exists()).unwrap_or(false)
    }

    fn path(&self) -> Option<&PathBuf> {
        self.path_override.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn set_override(&mut self, p: PathBuf) {
        self.path_override = Some(p);
    }
}

impl Default for StateStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("yushan_test_state_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("state.json")
    }

    #[test]
    fn test_load_nonexistent_returns_default() {
        let mut store = StateStore::new();
        store.set_override(temp_path("nonexistent"));
        let state = store.load();
        assert_eq!(state, AppState::default());
    }

    #[test]
    fn test_roundtrip() {
        let mut store = StateStore::new();
        let path = temp_path("roundtrip");
        store.set_override(path.clone());
        let state = AppState {
            last_active_provider: Some("minimax".into()),
            last_active_model: Some("abab-7".into()),
        };
        store.save(&state).unwrap();
        let loaded = store.load();
        assert_eq!(loaded, state);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn test_save_sets_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let mut store = StateStore::new();
        let path = temp_path("perms");
        store.set_override(path.clone());
        let state = AppState {
            last_active_provider: Some("x".into()),
            last_active_model: Some("y".into()),
        };
        store.save(&state).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_exists() {
        let mut store = StateStore::new();
        let path = temp_path("exists");
        store.set_override(path.clone());
        assert!(!store.exists());
        store.save(&AppState::default()).unwrap();
        assert!(store.exists());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
