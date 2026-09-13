//! 接线器（ADR-0010）：持有**会话 + 队列 + 模型 + 事件出口**。
//!
//! Agent 已降为无状态执行器；「当前是哪个会话」「当前用哪个模型」都归这里。
//! 调用点经 [`Wiring::ports`] 把这三样借给 `Agent::run`。
//!
//! **`/new` = 换队列**：替换 `session` + `inbox` 两个句柄——新会话文件 + 新空
//! `Inbox`（pending 丢弃），旧会话文件保留。Agent / BasicLoop / ys-channel
//! 全程不知情（设计 §3「`/new` 的语义」）。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ys_channel::Inbox;
use ys_event::EventSink;
use ys_model::Model;
use ys_runtime::AgentPorts;
use ys_session::{JsonlSession, MemorySession, Session, SessionError};
// `Session` trait 需在作用域内才能对 `Box<dyn Session>` 调方法。
use ys_session::Message;

/// 接线器：agent 之外的所有权所在。
///
/// 两种形态：
/// - **持久**（[`Wiring::persistent`]）：交互式；会话落 `sessions_dir/{id}.jsonl`，
///   启动时恢复最近一个。
/// - **一次性**（[`Wiring::ephemeral`]）：`-p`/`--json`；`MemorySession`，不落盘、
///   不恢复（保持单次运行行为与改动前一致）。
pub struct Wiring {
    model: Option<Box<dyn Model>>,
    session: Box<dyn Session>,
    inbox: Inbox,
    events: Box<dyn EventSink>,
    /// 会话落盘目录；`None` = 一次性会话。
    sessions_dir: Option<PathBuf>,
    /// 当前会话文件路径（持久形态）。
    session_path: Option<PathBuf>,
}

impl Wiring {
    /// 一次性会话：`MemorySession`，不落盘、不恢复。`-p`/`--json` 用。
    pub fn ephemeral(model: Option<Box<dyn Model>>, events: Box<dyn EventSink>) -> Self {
        Self {
            model,
            session: Box::new(MemorySession::new()),
            inbox: Inbox::new(),
            events,
            sessions_dir: None,
            session_path: None,
        }
    }

    /// 交互会话：恢复 `sessions_dir` 下最近的 `*.jsonl`（无则新建），后续落盘。
    pub async fn persistent(
        model: Option<Box<dyn Model>>,
        events: Box<dyn EventSink>,
        sessions_dir: PathBuf,
    ) -> Result<Self, SessionError> {
        std::fs::create_dir_all(&sessions_dir).map_err(|e| {
            SessionError::Storage(format!(
                "failed to create sessions dir {}: {e}",
                sessions_dir.display()
            ))
        })?;
        let path = latest_session_path(&sessions_dir)
            .unwrap_or_else(|| sessions_dir.join(format!("{}.jsonl", new_session_id())));
        let session = JsonlSession::open(&path).await?;
        Ok(Self {
            model,
            session: Box::new(session),
            inbox: Inbox::new(),
            events,
            sessions_dir: Some(sessions_dir),
            session_path: Some(path),
        })
    }

    /// **`/new`**：换新会话 + 新空 `Inbox`（pending 丢弃）；旧会话文件保留（不删）。
    ///
    /// 持久形态下 **先建好新会话并立即落盘**（空文件）：
    /// - `JsonlSession::open` 不预建文件，只在**首次追加**时落盘。若只 `open`
    ///   不落盘，用户 `/new` 后不发消息就退出，磁盘上仍只有旧文件，
    ///   重启时 [`latest_session_path`] 仍选中旧文件 —— `/new` 等于没生效。
    ///   故 `open` 后立即 `clear()` 触发一次落盘，使新文件名时间戳最新且真实存在。
    /// - 顺序上**先 open + clear、成功后才替换** `session` / `session_path` /
    ///   `inbox`：任一步失败时状态保持原样，不会出现「pending 已丢、会话未换」
    ///   的半新半旧。
    ///
    /// 返回新会话文件路径（一次性会话返回 `None`）。
    pub async fn new_session(&mut self) -> Result<Option<PathBuf>, SessionError> {
        match &self.sessions_dir {
            Some(dir) => {
                let path = dir.join(format!("{}.jsonl", new_session_id()));
                let mut session = JsonlSession::open(&path).await?;
                // 立即落盘空文件：`clear()` 会 flush_to_file，使文件真实存在。
                session.clear().await?;
                self.session = Box::new(session);
                self.session_path = Some(path.clone());
                self.inbox = Inbox::new(); // pending 丢弃（设计 §3：未处理输入失去语境）
                Ok(Some(path))
            }
            None => {
                self.session = Box::new(MemorySession::new());
                self.session_path = None;
                self.inbox = Inbox::new();
                Ok(None)
            }
        }
    }

    /// 清空当前会话的消息（`/compact` 的 MVP 语义）。
    pub async fn clear_session(&mut self) -> Result<(), SessionError> {
        self.session.clear().await
    }

    /// 借用端口给 `Agent::run` / `run_turn`。
    pub fn ports(&mut self) -> AgentPorts<'_> {
        AgentPorts {
            model: self.model.as_deref(),
            session: self.session.as_mut(),
            events: self.events.as_mut(),
        }
    }

    /// 当前 inbox 的克隆句柄（`Inbox` 内为 `Arc`，克隆共享同一底层队列）。
    pub fn inbox(&self) -> Inbox {
        self.inbox.clone()
    }

    pub fn is_configured(&self) -> bool {
        self.model.is_some()
    }

    /// 当前模型标识（供 `AppView` 快照；无 TUI 构建下不被读取）。
    #[cfg_attr(not(feature = "tui-ratatui"), allow(dead_code))]
    pub fn model_id(&self) -> Option<&str> {
        self.model.as_deref().map(|m| m.model_id())
    }

    /// 替换模型（`/login`、`/logout`、`/model` 经此，不再伸手进 Agent）。
    pub fn set_model(&mut self, model: Option<Box<dyn Model>>) {
        self.model = model;
    }

    /// 当前会话消息（供 `AppView` 快照；无 TUI 构建下不被读取）。
    #[cfg_attr(not(feature = "tui-ratatui"), allow(dead_code))]
    pub fn session_messages(&self) -> &[Message] {
        self.session.messages()
    }

    /// 当前会话文件路径（一次性会话为 `None`）。
    ///
    /// 生产路径当前不读（`/new` 用 `new_session` 的返回值），保留为公开只读
    /// 查询：`/status`、测试与后续「会话列表」需要它。
    #[allow(dead_code)]
    pub fn session_path(&self) -> Option<&Path> {
        self.session_path.as_deref()
    }
}

/// 会话 id：`{unix 秒}_{纳秒零填充}`。按字典序即时间序，便于「取最近一个」。
fn new_session_id() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch");
    format!("{}_{:09}", d.as_secs(), d.subsec_nanos())
}

/// 最近一个会话文件：文件名最大者（id 前缀保证字典序 == 时间序）。
fn latest_session_path(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ys_channel::Intent;
    use ys_core::{ContentBlock, Role};
    use ys_event::CollectingSink;

    fn text_message(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("yushan_wiring_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `/new`（一次性）：inbox 里的 pending 被丢弃、消息历史清空。
    #[tokio::test]
    async fn new_session_ephemeral_discards_pending() {
        let mut w = Wiring::ephemeral(None, Box::new(CollectingSink::new()));
        w.ports().session.append(text_message("old")).await.unwrap();
        w.inbox().push(text_message("pending"), Intent::FollowUp);
        assert!(!w.inbox().is_empty());

        let path = w.new_session().await.unwrap();

        assert!(path.is_none(), "一次性会话无文件");
        assert!(w.inbox().is_empty(), "pending 应被丢弃");
        assert!(w.session_messages().is_empty(), "历史应清空");
    }

    /// `/new`（持久）：旧文件保留且非空；新文件生成且为空；新 inbox 为空。
    #[tokio::test]
    async fn new_session_persistent_keeps_old_file_and_creates_empty_new() {
        let dir = temp_dir("new_session");
        let mut w = Wiring::persistent(None, Box::new(CollectingSink::new()), dir.clone())
            .await
            .unwrap();

        w.ports()
            .session
            .append(text_message("held in old"))
            .await
            .unwrap();
        let old_path = w.session_path().unwrap().to_path_buf();
        assert!(old_path.exists());
        w.inbox().push(text_message("pending"), Intent::FollowUp);

        let new_path = w.new_session().await.unwrap().expect("持久会话应返回路径");

        assert_ne!(old_path, new_path, "应生成新会话文件");
        assert!(old_path.exists(), "旧会话文件必须保留");
        let old_content = std::fs::read_to_string(&old_path).unwrap();
        assert!(
            old_content.contains("held in old"),
            "旧文件内容应保留: {old_content}"
        );
        // 新会话文件由 `/new` **预建**（open 后 clear 立即落盘），不待首次追加。
        assert!(new_path.exists(), "新会话文件应被预建");
        assert!(
            std::fs::read_to_string(&new_path).unwrap().is_empty(),
            "预建的新文件应为空"
        );
        assert!(w.session_messages().is_empty(), "新会话历史为空");
        assert!(w.inbox().is_empty(), "新 inbox 为空（pending 丢弃）");

        w.ports()
            .session
            .append(text_message("fresh"))
            .await
            .unwrap();
        let new_content = std::fs::read_to_string(&new_path).unwrap();
        assert!(
            new_content.contains("fresh") && !new_content.contains("held in old"),
            "新文件应只含新会话内容: {new_content}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `JsonlSession` 恢复：写入 → 重开 → 消息还在，且恢复的是最近一个文件。
    #[tokio::test]
    async fn persistent_recovers_latest_session() {
        let dir = temp_dir("recover");

        let path = {
            let mut w = Wiring::persistent(None, Box::new(CollectingSink::new()), dir.clone())
                .await
                .unwrap();
            w.ports()
                .session
                .append(text_message("persisted"))
                .await
                .unwrap();
            w.session_path().unwrap().to_path_buf()
        };

        let w2 = Wiring::persistent(None, Box::new(CollectingSink::new()), dir.clone())
            .await
            .unwrap();
        assert_eq!(w2.session_path(), Some(path.as_path()), "应恢复最近文件");
        assert_eq!(w2.session_messages().len(), 1, "消息应恢复");
        assert_eq!(w2.session_messages()[0].content.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 旧文件保留后，`/new` 再启动恢复的是**新**文件（最近者），不是旧的。
    #[tokio::test]
    async fn recovery_picks_newest_after_new() {
        let dir = temp_dir("recover_new");
        {
            let mut w = Wiring::persistent(None, Box::new(CollectingSink::new()), dir.clone())
                .await
                .unwrap();
            w.ports().session.append(text_message("old")).await.unwrap();
            w.new_session().await.unwrap();
            w.ports().session.append(text_message("new")).await.unwrap();
        }

        let w2 = Wiring::persistent(None, Box::new(CollectingSink::new()), dir.clone())
            .await
            .unwrap();
        assert_eq!(w2.session_messages().len(), 1);
        // 新会话只有 "new"。
        let text = match &w2.session_messages()[0].content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert_eq!(text, "new");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **回归**：`/new` 后**不发消息就退出**，重启必须恢复**新**（空）会话，
    /// 而不是含旧消息的会话。
    ///
    /// 修复前 `new_session` 只 `JsonlSession::open`（惰性落盘），磁盘上只有
    /// 旧文件 → `latest_session_path` 仍选旧文件 → `/new` 等于没生效。
    #[tokio::test]
    async fn restart_after_new_without_messages_recovers_new_empty_session() {
        let dir = temp_dir("recover_new_empty");
        let new_path = {
            let mut w = Wiring::persistent(None, Box::new(CollectingSink::new()), dir.clone())
                .await
                .unwrap();
            w.ports()
                .session
                .append(text_message("first msg"))
                .await
                .unwrap();
            let new_path = w.new_session().await.unwrap().expect("持久会话应返回路径");
            // 关键：不发任何消息就结束作用域（模拟退出）。
            assert!(new_path.exists(), "/new 应立即落盘空文件");
            new_path
        };

        // 重新构造 Wiring = 模拟重启。
        let w2 = Wiring::persistent(None, Box::new(CollectingSink::new()), dir.clone())
            .await
            .unwrap();
        assert_eq!(
            w2.session_path(),
            Some(new_path.as_path()),
            "重启应恢复新文件"
        );
        assert!(
            w2.session_messages().is_empty(),
            "重启应恢复新空会话，而非含 'first msg' 的旧会话"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 覆盖：`set_model` 后 `model_id()` 精确值、`is_configured()` 两态。
    #[test]
    fn set_model_updates_model_id_and_is_configured() {
        let mut w = Wiring::ephemeral(None, Box::new(CollectingSink::new()));
        assert!(!w.is_configured(), "无 model 时未配置");
        assert_eq!(w.model_id(), None);

        w.set_model(Some(Box::new(ys_model::MockModel::new("test-model"))));
        assert!(w.is_configured(), "有 model 时已配置");
        assert_eq!(w.model_id(), Some("test-model"), "model_id 应为精确值");

        w.set_model(None);
        assert!(!w.is_configured(), "移除 model 后未配置");
        assert_eq!(w.model_id(), None);
    }
}
