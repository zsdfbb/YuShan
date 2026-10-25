//! 会话状态持久化（state.json）。
//!
//! state 独立于 auth.json 存放——不含敏感数据，但为与 `ProviderRegistry::write_auth`
//! 保持一致仍限制为 0o600。
//!
//! 持久化字段（可扩展）：
//! - `last_active_provider` — 最近一次登录的 provider 名称
//! - `last_active_model` — 最近一次选中的 model 名称
//! - `theme` — 配色主题的 kebab-case 名（全局偏好，不进 session meta）

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct AppState {
    pub last_active_provider: Option<String>,
    pub last_active_model: Option<String>,
    /// 配色主题的 kebab-case 名（`ThemeKind::from_str` 的合法输入）。
    ///
    /// 旧 state.json 没有本键也必须能读（否则 `load()` 回落 `AppState::default()`，
    /// 把 `last_active_model` 一起清掉）。
    ///
    /// 显式写 `#[serde(default)]` 与仓内其余可选字段一致（`SessionMeta` 的
    /// `title` / `provider` / `model` 等）。**注意**：`Option<T>` 字段在 serde 里
    /// 缺省本就解成 `None`，该属性对它是冗余的 —— 它在此处是**风格一致性 + 意图声明**，
    /// 不是正确性开关。真正会因缺键失败的是**非 `Option` 且无默认值**的字段。
    #[serde(default)]
    pub theme: Option<String>,
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
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("yushan_test_state_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("state.json")
    }

    /// 并行安全的临时路径：名字含**进程内原子序号 + `process::id()`**。
    ///
    /// 写文件的用例用它（同名的 `temp_path` 在并行跑两个测试二进制时会撞目
    /// → 互删文件）。
    fn unique_temp_path(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "yushan_test_state_{tag}_{}_{seq}",
            std::process::id()
        ));
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
            ..Default::default()
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
            ..Default::default()
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

    /// **C9**：旧 `state.json`（**没有** `theme` 键）仍能完整解析，且不丢老字段。
    ///
    /// **必须能区分「解析成功」与「回落 default」** —— `load()` 在解析失败时静默返回
    /// `AppState::default()`，而默认值的 `theme` 恰好也是 `None`、`last_active_model`
    /// 也是 `None`。只断言「`theme == None`」的用例在两条路径上都绿，等于没测。
    ///
    /// 故这里放一个**只有解析成功才存在**的哨兵（`last_active_provider == Some("p")`），
    /// 并额外断言整份结果**不等于**默认值。这样一旦某天有人给 `AppState` 加一个
    /// 非 `Option` 且无默认值的必填字段（真正的兼容性风险），本用例立刻红。
    #[test]
    fn test_old_state_without_theme_keeps_last_model() {
        let mut store = StateStore::new();
        let path = unique_temp_path("legacy_no_theme");
        store.set_override(path.clone());
        // 逐字旧格式：只有两个老字段，**无** `theme` 键。
        std::fs::write(
            &path,
            r#"{"last_active_provider":"p","last_active_model":"m"}"#,
        )
        .unwrap();

        let state = store.load();
        // 哨兵：解析成功才有。若走了回落路径，这条立刻红。
        assert_eq!(
            state.last_active_provider.as_deref(),
            Some("p"),
            "缺 theme 键不得让整份 state 回落默认值"
        );
        assert_eq!(state.last_active_model.as_deref(), Some("m"));
        assert_eq!(state.theme, None, "旧文件就是没有主题偏好");
        assert_ne!(
            state,
            AppState::default(),
            "结果必须来自解析，而不是解析失败后的默认值"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// `theme` 与其他偏好在同一份 JSON 里共存，互不干扰。
    #[test]
    fn test_theme_roundtrip_alongside_model() {
        let mut store = StateStore::new();
        let path = unique_temp_path("theme_roundtrip");
        store.set_override(path.clone());
        let state = AppState {
            last_active_provider: Some("minimax".into()),
            last_active_model: Some("abab-7".into()),
            theme: Some("nord".into()),
        };
        store.save(&state).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"theme\""), "落盘 JSON 应含 theme 键：{raw}");
        assert_eq!(store.load(), state);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
