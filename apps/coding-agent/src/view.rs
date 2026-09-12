//! `AppView` —— 唯一的展示侧数据源。
//!
//! `format.rs::print_*` 函数接收 `&AppView`，对 `Config` / `Agent` / `ProviderRegistry`
//! 一无所知。当 `view_dirty = true`（`tui.rs` 在任何 command 执行或 turn 完成后设置）
//! 时，`AppView::from_sources` 重建快照。
//!
//! Grouping A 说明：`tools` 与 `context_window` 是占位（`Vec::new()` / `None`）。
//! 待 Grouping C 中 `Agent::tool_names()` 与 `Agent::context_window()` 落地后填充（ADR-0007）。

use std::path::PathBuf;
use std::time::Instant;

use ys_runtime::Agent;

use crate::config::Config;
use crate::provider::ProviderRegistry;
use crate::state::StateStore;
use crate::status::TurnStats;

/// 单个 slash command 的元数据，供 `/help` 输出和
/// ratatui Tab completer（ui/events.rs）使用。
#[derive(Clone, Debug)]
#[allow(dead_code)] // description / arg_hint 由 /help 与 ratatui Tab completer 消费
pub struct CommandMeta {
    pub name: &'static str,
    pub description: &'static str,
    pub arg_hint: Option<&'static str>,
}

/// 全部与展示相关的状态快照。克隆开销低（约 `10 个小字段`）。
#[derive(Clone, Debug)]
pub struct AppView {
    // 身份信息
    pub cwd: PathBuf,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub config_path: PathBuf,
    pub logged_in_providers: Vec<String>,
    pub total_known_providers: usize,
    pub version: &'static str,

    // 统计
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub turn_count: u32,

    // 会话
    pub session_started: Instant,
    /// 当前会话中的消息数。供未来的 "context full" 警告与 `/status` 读取；
    /// v0 渲染路径不消费。
    #[allow(dead_code)]
    pub message_count: usize,

    // 能力
    pub tools: Vec<String>,
    pub context_window: Option<usize>,
    pub is_first_run: bool,

    // 命令
    pub commands: Vec<CommandMeta>,
}

impl AppView {
    /// 从实时数据源构建快照。当 `view_dirty = true` 时由 `tui.rs` 调用。
    /// 调用方负责在重建之间保持 `stats` 与 `session_started` 一致
    /// （通常二者都由 `tui.rs` 持有）。
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
            commands: Vec::new(), // 由调用方填充
        }
    }

    /// 人类可读的会话时长：`Ns` / `Nm Ms` / `Nh Mm`。
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
        // 不足一分钟的渲染总以 "s" 结尾（如 "0s"、"12s"）。
        assert!(s.ends_with('s'), "expected trailing 's', got {s:?}");
        assert!(!s.contains(' '), "sub-minute form must not contain spaces");
    }

    #[test]
    fn test_command_meta_field_types_static() {
        // 编译期检查：&'static str 字段形态良好。
        let meta = CommandMeta {
            name: "help",
            description: "Show available commands",
            arg_hint: None,
        };
        assert_eq!(meta.name, "help");
        assert!(meta.arg_hint.is_none());
    }
}
