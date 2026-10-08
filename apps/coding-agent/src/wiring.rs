//! 接线器（ADR-0010）：持有**会话 + 模型 + 事件出口**。
//!
//! Agent 已降为无状态执行器；「当前是哪个会话」「当前用哪个模型」都归这里。
//! 调用点经 [`Wiring::ports`] 把这三样借给 `Agent::run_turn`。
//!
//! **默认新会话**：启动时 [`Wiring::persistent_new`] 只算出新文件名、**惰性落盘**
//! （不预建、不扫目录）；恢复只走显式 [`Wiring::resume`]（由 `main` 在起屏前
//! 解析 `--resume` 得到路径）。
//!
//! **`/new` = 换会话**：替换 `session` 句柄 —— 新会话文件（旧文件保留）。
//! Agent / BasicLoop 全程不知情。
//!
//! **不再有 `Inbox`**（T1 起删除）：未处理输入的唯一持有者是 UI 的输入框，
//! app 线程每收到一条 [`Request::Prompt`](ys_protocol::Request::Prompt) 就跑
//! 一个回合 —— 队列与历史的历史性合并已成过去（设计 §8 三条信道）。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ys_event::EventSink;
use ys_model::Model;
use ys_runtime::AgentPorts;
use ys_session::{JsonlSession, MemorySession, Session, SessionError, SessionMeta};
// `Session` trait 需在作用域内才能对 `Box<dyn Session>` 调方法。
use ys_session::Message;

/// 接线器：agent 之外的所有权所在。
///
/// 两种形态：
/// - **持久**（[`Wiring::persistent_new`] / [`Wiring::resume`]）：会话落
///   `sessions_dir/{id}.jsonl`。
/// - **一次性**（[`Wiring::ephemeral`]，仅测试）：`MemorySession`，不落盘、不恢复。
pub struct Wiring {
    model: Option<Box<dyn Model>>,
    session: Box<dyn Session>,
    events: Box<dyn EventSink>,
    /// 会话落盘目录；`None` = 一次性会话。
    sessions_dir: Option<PathBuf>,
    /// 当前会话文件路径（持久形态）。
    session_path: Option<PathBuf>,
}

impl Wiring {
    /// 一次性会话：`MemorySession`，不落盘、不恢复。
    ///
    /// 生产路径不再用它（`-p`/`--json` 已改持久化）；保留是 `MemorySession` 作为
    /// 「不落盘」构造的唯一入口，测试与 `new_session` 的一次性分支共用。
    #[cfg(test)]
    pub fn ephemeral(model: Option<Box<dyn Model>>, events: Box<dyn EventSink>) -> Self {
        Self {
            model,
            session: Box::new(MemorySession::new()),
            events,
            sessions_dir: None,
            session_path: None,
        }
    }

    /// 新会话（默认形态）：`create_dir_all` 后算出 `{unix}_{nanos}.jsonl`，
    /// `JsonlSession::open` **惰性**打开 —— 不预建文件、不扫目录，首次追加才落盘。
    pub async fn persistent_new(
        model: Option<Box<dyn Model>>,
        events: Box<dyn EventSink>,
        sessions_dir: PathBuf,
    ) -> Result<Self, SessionError> {
        Self::persistent_at(model, events, sessions_dir, None).await
    }

    /// 恢复指定会话文件（显式 `--resume`）：读回其 meta 与消息。
    pub async fn resume(
        model: Option<Box<dyn Model>>,
        events: Box<dyn EventSink>,
        sessions_dir: PathBuf,
        path: PathBuf,
    ) -> Result<Self, SessionError> {
        Self::persistent_at(model, events, sessions_dir, Some(path)).await
    }

    /// 持久形态的公共构造。`path` 为 `Some` 即恢复该文件，`None` 即新文件名。
    ///
    /// `create_dir_all` 必须在**两个**入口都发生：惰性落盘时父目录不存在会让首次
    /// `flush_to_file` 的 rename 失败（`/new` 换出的目录同理）。
    async fn persistent_at(
        model: Option<Box<dyn Model>>,
        events: Box<dyn EventSink>,
        sessions_dir: PathBuf,
        path: Option<PathBuf>,
    ) -> Result<Self, SessionError> {
        std::fs::create_dir_all(&sessions_dir).map_err(|e| {
            SessionError::Storage(format!(
                "failed to create sessions dir {}: {e}",
                sessions_dir.display()
            ))
        })?;
        let path = path.unwrap_or_else(|| sessions_dir.join(format!("{}.jsonl", new_session_id())));
        let session = JsonlSession::open(&path).await?;
        Ok(Self {
            model,
            session: Box::new(session),
            events,
            sessions_dir: Some(sessions_dir),
            session_path: Some(path),
        })
    }

    /// **`/new`**：换新会话；旧会话文件保留（不删）。
    ///
    /// 只 `JsonlSession::open`（**惰性**，不预建空文件）：新会话首次追加才落盘。
    /// 顺序上**先 open 成功后才替换** `session` / `session_path`：任一步失败时状态
    /// 保持原样，不会出现「会话未换却已丢历史」的半新半旧。
    ///
    /// 返回新会话文件路径（一次性会话返回 `None`）。
    pub async fn new_session(&mut self) -> Result<Option<PathBuf>, SessionError> {
        match &self.sessions_dir {
            Some(dir) => {
                let path = dir.join(format!("{}.jsonl", new_session_id()));
                let session = JsonlSession::open(&path).await?;
                self.session = Box::new(session);
                self.session_path = Some(path.clone());
                Ok(Some(path))
            }
            None => {
                self.session = Box::new(MemorySession::new());
                self.session_path = None;
                Ok(None)
            }
        }
    }

    /// 借用端口给 `Agent::run_turn`。
    ///
    /// 轮边界源**不在这里**：它由 `app_loop` 每回合新建、经 [`AgentPorts::new`]
    /// 就地传入（`Abort` 的标志位必须随回合作废）。
    pub fn ports(&mut self) -> AgentPorts<'_> {
        AgentPorts::new(
            self.model.as_deref(),
            self.session.as_mut(),
            self.events.as_mut(),
            None,
        )
    }

    pub fn is_configured(&self) -> bool {
        self.model.is_some()
    }

    /// 当前模型标识（构造 `CodingView` 快照用）。
    pub fn model_id(&self) -> Option<&str> {
        self.model.as_deref().map(|m| m.model_id())
    }

    /// 替换模型（`/login`、`/logout`、`/model` 经此，不再伸手进 Agent）。
    pub fn set_model(&mut self, model: Option<Box<dyn Model>>) {
        self.model = model;
    }

    /// 当前会话消息（构造 `CodingView` 快照用）。
    pub fn session_messages(&self) -> &[Message] {
        self.session.messages()
    }

    /// 当前会话文件路径（一次性会话为 `None`）。
    ///
    /// 消费者：`CodingView.session_path` 与 `/export` 的默认落点。
    pub fn session_path(&self) -> Option<&Path> {
        self.session_path.as_deref()
    }

    /// 当前会话元数据（`None` = 尚无首行 meta）。
    pub fn meta(&self) -> Option<&SessionMeta> {
        self.session.meta()
    }

    /// 就地修改 meta 并落盘。
    ///
    /// 缺省 meta 必须用 [`SessionMeta::new`] 起头（`v = 1`）—— 不能用
    /// `SessionMeta::default()`，后者的 `v = 0` 与 serde 缺省 `1` 不一致。
    pub async fn update_meta(
        &mut self,
        f: impl FnOnce(&mut SessionMeta),
    ) -> Result<(), SessionError> {
        let mut meta = self
            .session
            .meta()
            .cloned()
            .unwrap_or_else(|| SessionMeta::new(now_unix()));
        f(&mut meta);
        self.session.set_meta(meta).await
    }
}

/// 会话 id：`{unix 秒}_{纳秒零填充}`。按字典序即时间序。
fn new_session_id() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch");
    format!("{}_{:09}", d.as_secs(), d.subsec_nanos())
}

/// 当前 Unix 秒（新建 meta 的 `created_at`）。
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ys_core::{ContentBlock, Role};
    use ys_event::CollectingSink;

    fn text_message(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// 临时目录名含**进程内原子序号 + `process::id()`**（并行安全）。
    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("yushan_wiring_{name}_{}_{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `/new`（一次性）：历史清空、无会话文件。
    #[tokio::test]
    async fn new_session_ephemeral_clears_history() {
        let mut w = Wiring::ephemeral(None, Box::new(CollectingSink::new()));
        w.ports().session.append(text_message("old")).await.unwrap();

        let path = w.new_session().await.unwrap();

        assert!(path.is_none(), "一次性会话无文件");
        assert!(w.session_messages().is_empty(), "历史应清空");
    }

    /// `/new`（持久）：旧文件保留；新会话**惰性**（不发消息则不落盘）。
    #[tokio::test]
    async fn new_session_persistent_keeps_old_file_and_is_lazy() {
        let dir = temp_dir("new_session");
        let mut w = Wiring::persistent_new(None, Box::new(CollectingSink::new()), dir.clone())
            .await
            .unwrap();

        w.ports()
            .session
            .append(text_message("held in old"))
            .await
            .unwrap();
        let old_path = w.session_path().unwrap().to_path_buf();
        assert!(old_path.exists());

        let new_path = w.new_session().await.unwrap().expect("持久会话应返回路径");

        assert_ne!(old_path, new_path, "应生成新会话文件");
        assert!(old_path.exists(), "旧会话文件必须保留");
        assert!(
            std::fs::read_to_string(&old_path)
                .unwrap()
                .contains("held in old"),
            "旧文件内容应保留"
        );
        // 惰性：`/new` 只换句柄，不预建空文件。
        assert!(!new_path.exists(), "未追加前不应落盘新文件");
        assert!(w.session_messages().is_empty(), "新会话历史为空");

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

    /// `persistent_new` **惰性**：构造后文件不存在，首次 append 才落盘。
    #[tokio::test]
    async fn persistent_new_is_lazy_until_first_append() {
        let dir = temp_dir("lazy_new");
        let mut w = Wiring::persistent_new(None, Box::new(CollectingSink::new()), dir.clone())
            .await
            .unwrap();
        let path = w.session_path().unwrap().to_path_buf();
        assert!(!path.exists(), "构造（open）不得预建文件");

        w.ports()
            .session
            .append(text_message("first"))
            .await
            .unwrap();
        assert!(path.exists(), "首次 append 才落盘");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `persistent_new` 不扫目录：即使目录里已有别的新会话，也另起一个新文件。
    #[tokio::test]
    async fn persistent_new_does_not_scan_existing_sessions() {
        let dir = temp_dir("no_scan");
        {
            let mut w = Wiring::persistent_new(None, Box::new(CollectingSink::new()), dir.clone())
                .await
                .unwrap();
            w.ports()
                .session
                .append(text_message("existing"))
                .await
                .unwrap();
        }

        let w2 = Wiring::persistent_new(None, Box::new(CollectingSink::new()), dir.clone())
            .await
            .unwrap();
        assert!(w2.session_messages().is_empty(), "默认新会话不恢复旧内容");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `resume` 读回指定文件的 meta 与消息。
    #[tokio::test]
    async fn resume_reads_back_meta_and_messages() {
        let dir = temp_dir("resume_meta");
        let path = {
            let mut w = Wiring::persistent_new(None, Box::new(CollectingSink::new()), dir.clone())
                .await
                .unwrap();
            w.update_meta(|m| {
                m.title = Some("标题".into());
                m.provider = Some("deepseek".into());
                m.model = Some("deepseek-chat".into());
                m.thinking_visible = true;
                m.input_tokens = 10;
                m.output_tokens = 3;
                m.turns = 2;
            })
            .await
            .unwrap();
            w.ports()
                .session
                .append(text_message("persisted"))
                .await
                .unwrap();
            w.session_path().unwrap().to_path_buf()
        };

        let w2 = Wiring::resume(
            None,
            Box::new(CollectingSink::new()),
            dir.clone(),
            path.clone(),
        )
        .await
        .unwrap();
        assert_eq!(w2.session_path(), Some(path.as_path()));
        assert_eq!(w2.session_messages().len(), 1, "消息应恢复");
        let meta = w2.meta().expect("meta 应恢复");
        assert_eq!(meta.title.as_deref(), Some("标题"));
        assert_eq!(meta.provider.as_deref(), Some("deepseek"));
        assert!(meta.thinking_visible);
        assert_eq!(meta.input_tokens, 10);
        assert_eq!(meta.turns, 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `update_meta` 幂等：title 只写一次，后续刷新不覆盖；缺省 meta 用 `v = 1`。
    #[tokio::test]
    async fn update_meta_title_is_written_once() {
        let dir = temp_dir("meta_idempotent");
        let mut w = Wiring::persistent_new(None, Box::new(CollectingSink::new()), dir.clone())
            .await
            .unwrap();

        w.update_meta(|m| {
            if m.title.is_none() {
                m.title = Some("第一次".into());
            }
            m.turns = 1;
        })
        .await
        .unwrap();
        let meta = w.meta().unwrap();
        assert_eq!(meta.title.as_deref(), Some("第一次"));
        assert_eq!(meta.v, 1, "缺省 meta 必须 v=1（不能用 default 的 v=0）");

        w.update_meta(|m| {
            if m.title.is_none() {
                m.title = Some("第二次".into());
            }
            m.turns = 2;
        })
        .await
        .unwrap();
        let meta = w.meta().unwrap();
        assert_eq!(meta.title.as_deref(), Some("第一次"), "title 只写一次");
        assert_eq!(meta.turns, 2, "其余字段照常刷新");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Session::clear`：消息清空，但会话文件仍在（同一个文件）。
    ///
    /// 说明：`Wiring` **不再**暴露 `clear_session`（U6b 起 `/compact` 走真压缩）。
    /// 保留本用例是为了钉住底层 `clear` 的语义 —— 它是 `/new` 与压缩写回共同的依赖。
    #[tokio::test]
    async fn session_clear_empties_history_but_keeps_file() {
        let dir = temp_dir("clear");
        let mut w = Wiring::persistent_new(None, Box::new(CollectingSink::new()), dir.clone())
            .await
            .unwrap();
        w.ports()
            .session
            .append(text_message("gone"))
            .await
            .unwrap();
        let path = w.session_path().unwrap().to_path_buf();

        w.ports().session.clear().await.unwrap();

        assert!(w.session_messages().is_empty());
        assert_eq!(w.session_path(), Some(path.as_path()));
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

    /// `ports()` 不携带边界源（边界由 `app_loop` 每回合新建）。
    #[test]
    fn ports_carry_no_boundary_source() {
        let mut w = Wiring::ephemeral(None, Box::new(CollectingSink::new()));
        assert!(
            w.ports().boundary.is_none(),
            "接线器不持有边界源 —— 它随回合生灭"
        );
    }
}
