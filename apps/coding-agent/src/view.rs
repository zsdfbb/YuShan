//! `AppView` — the sole display-side data source.
//!
//! `format.rs::print_*` functions take `&AppView` and know nothing about
//! `Config` / `Agent` / `ProviderRegistry`. `AppView::from_sources` rebuilds
//! the snapshot whenever `view_dirty = true` (set by `tui.rs` after any
//! command execution or completed turn).
//!
//! Grouping A note: `tools` and `context_window` are placeholders (`Vec::new()`
//! / `None`). They will be populated in grouping C once `Agent::tool_names()`
//! and `Agent::context_window()` land (ADR-0007).

use std::path::PathBuf;
use std::time::Instant;

use agent_runtime::Agent;

use crate::config::Config;
use crate::provider::ProviderRegistry;
use crate::state::StateStore;
use crate::status::TurnStats;

/// Metadata for one slash command, used by `/help` output and the
/// completer (groups E / D).
#[derive(Clone, Debug)]
#[allow(dead_code)] // description / arg_hint are consumed by /help and the completer
// (tui_completer + format.rs); the v0 code path doesn't reach
// them yet but the fields are part of the public data model.
pub struct CommandMeta {
    pub name: &'static str,
    pub description: &'static str,
    pub arg_hint: Option<&'static str>,
}

/// Snapshot of all display-relevant state. Cheap to clone (`~10 small fields`).
#[derive(Clone, Debug)]
pub struct AppView {
    // identity
    pub cwd: PathBuf,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub config_path: PathBuf,
    pub logged_in_providers: Vec<String>,
    pub total_known_providers: usize,
    pub version: &'static str,

    // stats
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub turn_count: u32,

    // session
    pub session_started: Instant,
    /// Number of messages in the current session. Read by future
    /// "context full" warnings and `/status`; not consumed by v0 render paths.
    #[allow(dead_code)]
    pub message_count: usize,

    // capabilities
    pub tools: Vec<String>,
    pub context_window: Option<usize>,
    pub is_first_run: bool,

    // commands
    pub commands: Vec<CommandMeta>,
}

impl AppView {
    /// Build a snapshot from the live sources. Called by `tui.rs` whenever
    /// `view_dirty = true`. The caller is responsible for keeping `stats` and
    /// `session_started` consistent across rebuilds (typically both are owned
    /// by `tui.rs`).
    pub fn from_sources(
        cfg: &Config,
        agent: &Agent,
        registry: &ProviderRegistry,
        state: &StateStore,
        stats: &TurnStats,
        session_started: Instant,
    ) -> Self {
        let logged_in = registry.logged_in_providers();
        let is_first_run = logged_in.is_empty() && !state.exists();
        Self {
            cwd: cfg.cwd.clone(),
            provider: cfg.provider.clone(),
            model: agent.model_id().map(String::from),
            config_path: registry.auth_path(),
            logged_in_providers: logged_in,
            total_known_providers: registry.providers().len(),
            version: env!("CARGO_PKG_VERSION"),
            total_input_tokens: stats.total_input_tokens,
            total_output_tokens: stats.total_output_tokens,
            turn_count: stats.turn_count,
            session_started,
            message_count: agent.session_messages().len(),
            tools: agent.tool_names(),
            context_window: Some(agent.context_window()),
            is_first_run,
            commands: Vec::new(), // populated by caller
        }
    }

    /// Human-readable session uptime: `Ns` / `Nm Ms` / `Nh Mm`.
    pub fn session_duration_str(&self) -> String {
        let secs = self.session_started.elapsed().as_secs();
        if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            format!("{}m {}s", secs / 60, secs % 60)
        } else {
            format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_view(started: Instant) -> AppView {
        AppView {
            cwd: PathBuf::from("/tmp"),
            provider: None,
            model: None,
            config_path: PathBuf::from("/x.json"),
            logged_in_providers: vec![],
            total_known_providers: 0,
            version: "0.0.0",
            total_input_tokens: 0,
            total_output_tokens: 0,
            turn_count: 0,
            session_started: started,
            message_count: 0,
            tools: vec![],
            context_window: None,
            is_first_run: false,
            commands: vec![],
        }
    }

    #[test]
    fn test_appview_default_fields() {
        let view = make_view(Instant::now());
        assert_eq!(view.total_input_tokens, 0);
        assert_eq!(view.total_output_tokens, 0);
        assert_eq!(view.turn_count, 0);
        assert_eq!(view.message_count, 0);
        assert!(view.logged_in_providers.is_empty());
        assert!(view.tools.is_empty());
        assert!(view.context_window.is_none());
    }

    #[test]
    fn test_session_duration_str_sub_minute() {
        let view = make_view(Instant::now());
        let s = view.session_duration_str();
        // Sub-minute rendering always ends with "s" (e.g., "0s", "12s").
        assert!(s.ends_with('s'), "expected trailing 's', got {s:?}");
        assert!(!s.contains(' '), "sub-minute form must not contain spaces");
    }

    #[test]
    fn test_command_meta_field_types_static() {
        // Compile-time check: &'static str fields are well-formed.
        let meta = CommandMeta {
            name: "help",
            description: "Show available commands",
            arg_hint: None,
        };
        assert_eq!(meta.name, "help");
        assert!(meta.arg_hint.is_none());
    }
}
