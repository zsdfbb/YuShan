//! `TodoStore` —— 清单的**唯一真值**：内存状态 + 落盘 + 两条硬规则。
//!
//! # 为什么真值不在会话里
//!
//! 会话消息会被压缩（`compact.rs` 摘的正是旧消息），任务越长，「最后一次 `todo` 调用的
//! `arguments`」越早变成旧消息。真值必须活在压缩够不着的地方，因此落成会话同级的独立文件
//! `<sessions_dir>/<stem>.todo.json`：重启时随会话按同一 stem 找到，**零新目录、零新环境变量**。
//!
//! # 两条硬规则都在 [`TodoStore::apply`] 里
//!
//! 1. **拒绝丢弃既有非终态项** —— 否则模型发一个 `[]` 就关掉了整个追问闭环；
//! 2. **非空且全部终态 ⇒ 清空 + 删文件** —— 判据是「无 open 项」而非「全部 `Completed`」，
//!    后者会让被**放弃**的项永久非终态 ⇒ 永不满足 ⇒ 永不删除。
//!
//! 执行者是**代码**，不是模型自觉：任何写入者（工具、将来的 `/todos`、`/new`）都经过这里，
//! 因此不可能留下一个已关闭的计划。
//!
//! # 落盘是**尽力而为**
//!
//! `apply` 是同步签名（设计 §7.2），写文件走 `std::fs` 的原子替换。写失败**不回滚内存**：
//! 内存是本次进程的真值，磁盘只是下次启动的起点 —— 为一次写失败丢掉模型刚写好的计划，
//! 代价远大于「重启后少一份清单」。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use crate::item::{TodoItem, normalize_content};

/// 会话文件旁边的那份清单：`<stem>.jsonl` ⇒ `<stem>.todo.json`。
///
/// 换后缀而非换目录：`/new` 换 stem ⇒ 计划天然归零；`Wiring::resume` 按同一 stem 开回
/// ⇒ 计划随之回来。两条既有路径都不用改。
pub const TODO_FILE_SUFFIX: &str = ".todo.json";

/// 落盘文件的外层形状。**带版本号**：将来改格式时读侧能识别旧文件而不是静默误读。
#[derive(Debug, Serialize, Deserialize)]
struct FileBody {
    version: u32,
    items: Vec<TodoItem>,
}

/// 当前落盘格式版本。
const FILE_VERSION: u32 = 1;

/// `apply` 的两种结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// 清单被更新（可能为空 ⇒ 文件已删）。
    Updated,
    /// 全部项都已终结 ⇒ 已清空并删文件。工具结果必须**显式**告诉模型这件事，
    /// 否则它会困惑于「我的列表去哪了」。
    ClearedAllDone,
}

/// 写入被拒绝的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TodoError {
    /// 入参里有既不在旧表里、也不属于本轮的项 —— 即模型想**丢弃既有非终态项**。
    WouldDropOpenItems { dropped: Vec<String> },
    /// 入参含空白内容（归一化后为空）。
    EmptyContent,
}

impl std::fmt::Display for TodoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WouldDropOpenItems { dropped } => write!(
                f,
                "拒绝写入：这会丢弃仍在进行中的计划项 {}。计划只能被推进，不能被删除 —— \
                 不做的事请标成 `cancelled`（而不是从列表里删掉）。",
                quote_list(dropped)
            ),
            Self::EmptyContent => {
                write!(f, "拒绝写入：`content` 不能为空白。")
            }
        }
    }
}

impl std::error::Error for TodoError {}

/// 把待引用的内容拼成一句人类可读的清单（最多 3 条，超出只报条数）。
fn quote_list(items: &[String]) -> String {
    const SHOWN: usize = 3;
    let shown: Vec<String> = items
        .iter()
        .take(SHOWN)
        .map(|content| format!("`{}`", truncate_for_error(content)))
        .collect();
    if items.len() > SHOWN {
        format!("{} 等 {} 项", shown.join("、"), items.len())
    } else {
        shown.join("、")
    }
}

/// 错误消息里的内容截断（按字符，避免切碎多字节字符）。
fn truncate_for_error(content: &str) -> String {
    const MAX: usize = 60;
    match content.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}…", &content[..cut]),
        None => content.to_string(),
    }
}

/// 清单的**唯一真值**。
///
/// 生命周期由接线器持有（`Arc`），`/new` 时就地改指向 ⇒ **同一 `Arc` 身份不变**，
/// 工具持的引用不会过期。
pub struct TodoStore {
    inner: Mutex<Inner>,
}

struct Inner {
    items: Vec<TodoItem>,
    /// 内容版本号：**只在内容真的变化时**递增，进程内单调，**不落盘**。
    ///
    /// 不落盘是刻意的：重启后的版本号从 0 重新开始，注入判据另有「会话里找不到标记」那条
    /// 自愈分支兜底，不需要跨进程可比。
    revision: u64,
    /// 落盘目标。`None` ⇒ 纯内存（`-p` / `--json` 的一次性会话）。
    path: Option<PathBuf>,
    /// 启动时读到坏文件留下的降级提示。**只报一次**（首次取走后清空），
    /// 免得每次工具调用都重复同一句话。
    degraded: Option<String>,
}

impl TodoStore {
    /// `Some(session_path)` ⇒ `<sessions_dir>/<stem>.todo.json`；`None`（`-p` / `--json`）⇒ 纯内存。
    ///
    /// 文件损坏 ⇒ 空列表 + 一句降级提示，**绝不 panic、绝不阻断启动**：
    /// 清单是辅助状态，为它挡住整个进程启动是拿主功能换副功能。
    pub fn open(session_path: Option<&Path>) -> Arc<Self> {
        let path = session_path.map(todo_path_for);
        let mut degraded = None;
        let items = match path.as_deref().map(load) {
            Some(Ok(items)) => items,
            Some(Err(reason)) => {
                degraded = Some(reason);
                Vec::new()
            }
            None => Vec::new(),
        };

        Arc::new(Self {
            inner: Mutex::new(Inner {
                items,
                revision: 0,
                path,
                degraded,
            }),
        })
    }

    /// 当前清单的快照（克隆）。
    pub fn snapshot(&self) -> Vec<TodoItem> {
        self.lock().items.clone()
    }

    /// 内容版本号，进程内单调、不落盘。
    pub fn revision(&self) -> u64 {
        self.lock().revision
    }

    /// 存在非终态项。**推进判据与删除判据共用它**。
    pub fn has_open_items(&self) -> bool {
        self.lock()
            .items
            .iter()
            .any(|item| !item.status.is_terminal())
    }

    /// 取走启动时的降级提示（**只给一次**）。
    ///
    /// 由接线器决定去处（stderr / `Outbound`）—— 本 crate 绝不 `print`。
    pub fn take_degraded_hint(&self) -> Option<String> {
        self.lock().degraded.take()
    }

    /// 全量重写（照 Claude Code `TodoWrite` 的「每次传完整列表」）。
    ///
    /// **两条硬规则（都在这里，不靠模型自觉）**：
    /// 1. **拒绝丢弃既有非终态项** —— 否则模型发一个 `[]` 就能关掉追问护栏；
    /// 2. **清单非空且全部终态 ⇒ 清空 + 删文件** —— 「全部完成后删除」的判据是
    ///    「无 open 项」而非「全部 `Completed`」，后者会让放弃的项永远卡住。
    ///
    /// 被拒绝时**状态一字不改**（含 `revision` 与文件）。
    pub fn apply(&self, incoming: Vec<TodoItem>) -> Result<ApplyOutcome, TodoError> {
        // 归一化后去重：同一内容出现两次按第一次的状态算（模型不该这么写，
        // 但「重复即覆盖」比「同一项出现两行」更接近它的本意）。
        let mut normalized: Vec<TodoItem> = Vec::with_capacity(incoming.len());
        for item in incoming {
            let content = normalize_content(&item.content);
            if content.is_empty() {
                return Err(TodoError::EmptyContent);
            }
            if normalized.iter().any(|kept| kept.content == content) {
                continue;
            }
            normalized.push(TodoItem {
                content,
                status: item.status,
            });
        }

        let mut inner = self.lock();

        // ── 硬规则 1：不得丢弃既有非终态项 ──────────────────────────────
        // 判据只看**旧表的 open 项是否都还在新表里**。
        // 已终结的项可以随时删：删掉一个终态项不改变「还有多少事没做完」，
        // 拦它只会把「整理历史」这种无害动作也变成错误。
        let dropped_open: Vec<String> = inner
            .items
            .iter()
            .filter(|old| !old.status.is_terminal())
            .filter(|old| !normalized.iter().any(|new| new.content == old.content))
            .map(|old| old.content.clone())
            .collect();
        if !dropped_open.is_empty() {
            return Err(TodoError::WouldDropOpenItems {
                dropped: dropped_open,
            });
        }

        // ── 硬规则 2：无 open 项 ⇒ 清空 + 删文件 ────────────────────────
        // 判据是「无 open 项」而非「全部 `Completed`」：被放弃（`cancelled`）的项
        // 同样终结，否则它会永远卡住删除条件。
        let all_terminal = normalized.iter().all(|item| item.status.is_terminal());
        if all_terminal && (!normalized.is_empty() || !inner.items.is_empty()) {
            let changed = !inner.items.is_empty();
            inner.items.clear();
            if changed {
                inner.revision += 1;
            }
            let path = inner.path.clone();
            drop(inner);
            remove_file(&path);
            return Ok(ApplyOutcome::ClearedAllDone);
        }

        let changed = inner.items != normalized;
        if changed {
            inner.items = normalized;
            inner.revision += 1;
        }
        let path = inner.path.clone();
        let items = inner.items.clone();
        drop(inner);

        if changed {
            save(path.as_deref(), &items);
        }
        Ok(ApplyOutcome::Updated)
    }

    /// `/new`：清空 + 换路径。**同一 `Arc` 身份不变** —— 工具持的就是它。
    ///
    /// 旧文件**删掉**：`/new` 的语义是「计划归零」，留一份孤儿文件只会让下次同名会话
    /// 读到上一次的计划。
    pub fn retarget(&self, session_path: Option<&Path>) {
        let new_path = session_path.map(todo_path_for);
        let mut inner = self.lock();
        let old_path = inner.path.clone();
        let changed = !inner.items.is_empty();
        inner.items.clear();
        if changed {
            inner.revision += 1;
        }
        inner.degraded = None;
        inner.path = new_path;
        drop(inner);

        remove_file(&old_path);
    }

    /// 落盘路径（测试与接线器诊断用）。`None` ⇒ 纯内存。
    pub fn path(&self) -> Option<PathBuf> {
        self.lock().path.clone()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // 锁中毒 ⇒ 前一个持锁者 panic 了。清单是「丢了也能重建」的辅助状态，
        // 因此**继续用**（数据仍自洽：所有写入都在锁内完成），而不是把 panic 传播给每个调用方。
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// 会话路径 ⇒ 清单路径（同目录、同 stem、换后缀）。
fn todo_path_for(session_path: &Path) -> PathBuf {
    let stem = session_path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session".to_string());
    match session_path.parent() {
        Some(dir) => dir.join(format!("{stem}{TODO_FILE_SUFFIX}")),
        None => PathBuf::from(format!("{stem}{TODO_FILE_SUFFIX}")),
    }
}

/// 读盘。`Err` 是**降级提示**（一句中文说明），不是给调用方处理的错误类型 ——
/// 调用方一律退化为空列表。
fn load(path: &Path) -> Result<Vec<TodoItem>, String> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        // 不存在 = 全新会话，不是降级。
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("计划文件读取失败（{}）：{err}", path.display())),
    };
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    match serde_json::from_str::<FileBody>(&raw) {
        Ok(body) if body.version == FILE_VERSION => Ok(body.items),
        Ok(body) => Err(format!(
            "计划文件版本不受支持（{}，version={}），已按空计划继续",
            path.display(),
            body.version
        )),
        Err(err) => Err(format!(
            "计划文件损坏（{}）：{err}；已按空计划继续",
            path.display()
        )),
    }
}

/// 原子写：先写同目录临时文件再 `rename`。中途崩溃只会留下一个临时文件，
/// 不会让读者看到半份 JSON。
///
/// 写失败**静默**：内存是本次进程的真值（见模块头）。临时文件名含进程内序号 ——
/// 并行测试可能同时写同一目录，撞名会让 `rename` 互相覆盖。
fn save(path: Option<&Path>, items: &[TodoItem]) {
    let Some(path) = path else { return };
    let body = FileBody {
        version: FILE_VERSION,
        items: items.to_vec(),
    };
    let Ok(json) = serde_json::to_string_pretty(&body) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp{seq}"));
    if fs::write(&tmp, json.as_bytes()).is_err() {
        let _ = fs::remove_file(&tmp);
        return;
    }
    if fs::rename(&tmp, path).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

/// 删文件；不存在视为成功。
fn remove_file(path: &Option<PathBuf>) {
    let Some(path) = path else { return };
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        // 删不掉就留着 —— 下次启动会读到它。不 panic：删文件失败不该让一次工具调用失败。
        Err(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::TodoStatus;
    use crate::test_support::test_dir;

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem::new(content, status)
    }

    fn todo_file(dir: &Path, stem: &str) -> PathBuf {
        dir.join(format!("{stem}{TODO_FILE_SUFFIX}"))
    }

    /// `open(None)` ⇒ 纯内存：写进去读得回，但**不产生任何文件**。
    ///
    /// `-p` / `--json` 走这条路径 —— 一次性会话不该在磁盘上留痕。
    #[test]
    fn memory_only_store_writes_no_file() {
        let dir = test_dir("memory_only");
        let store = TodoStore::open(None);

        assert!(store.path().is_none(), "None ⇒ 无落盘目标");
        assert_eq!(
            store.apply(vec![item("a", TodoStatus::Pending)]),
            Ok(ApplyOutcome::Updated)
        );
        assert_eq!(store.snapshot(), vec![item("a", TodoStatus::Pending)]);
        assert_eq!(
            std::fs::read_dir(&dir).expect("读临时目录").count(),
            0,
            "纯内存 store 不得落盘"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 路径推导：`<stem>.jsonl` ⇒ 同目录 `<stem>.todo.json`。
    #[test]
    fn todo_path_shares_stem_and_dir() {
        assert_eq!(
            todo_path_for(Path::new("/s/2026_abc.jsonl")),
            PathBuf::from("/s/2026_abc.todo.json")
        );
        // 无父目录（相对文件名）也能推导。
        assert_eq!(
            todo_path_for(Path::new("abc.jsonl")),
            PathBuf::from("abc.todo.json")
        );
    }

    // ── 硬规则 1：拒绝丢弃既有非终态项 ──────────────────────────────────

    /// `apply([])` 丢弃 `Pending` / `InProgress` 项 ⇒ **拒绝**，且状态一字不改。
    ///
    /// 变异：删掉 `dropped_open` 的检查 ⇒ 模型发一个 `[]` 就能关掉整个追问闭环 ⇒ 本用例变红。
    /// 这是 ADR-0024 决策 4 的「防逃逸」正身。
    #[test]
    fn apply_empty_rejects_dropping_open_items() {
        let dir = test_dir("drop_empty");
        let path = dir.join("s.jsonl");
        let store = TodoStore::open(Some(&path));
        let before = vec![
            item("待办项", TodoStatus::Pending),
            item("进行中", TodoStatus::InProgress),
            item("已做完", TodoStatus::Completed),
        ];
        store.apply(before.clone()).expect("首次写入");
        let revision = store.revision();
        let on_disk = fs::read_to_string(todo_file(&dir, "s")).expect("文件已落盘");

        let err = store.apply(Vec::new()).expect_err("丢弃 open 项必须被拒绝");

        match err {
            TodoError::WouldDropOpenItems { dropped } => {
                assert_eq!(dropped, vec!["待办项".to_string(), "进行中".to_string()]);
            }
            other => panic!("错误类型不对：{other:?}"),
        }
        assert_eq!(store.snapshot(), before, "被拒绝时状态不得改变");
        assert_eq!(store.revision(), revision, "被拒绝时版本号不得递增");
        assert_eq!(
            fs::read_to_string(todo_file(&dir, "s")).expect("文件仍在"),
            on_disk,
            "被拒绝时文件不得被改写"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 只删掉**其中一个** `Pending` 项也拒绝（不是只有 `[]` 才拦）。
    ///
    /// 变异：把判据写成「新表为空才拦」⇒ 本用例变红。
    #[test]
    fn dropping_one_open_item_is_rejected() {
        let store = TodoStore::open(None);
        store
            .apply(vec![
                item("a", TodoStatus::Pending),
                item("b", TodoStatus::InProgress),
            ])
            .expect("首次写入");

        let err = store
            .apply(vec![item("a", TodoStatus::Pending)])
            .expect_err("删掉 b 必须被拒绝");
        assert!(matches!(err, TodoError::WouldDropOpenItems { .. }));
        assert_eq!(store.snapshot().len(), 2, "状态不变");
    }

    /// 删除**已终结**的项是允许的：它不改变「还有多少事没做完」。
    ///
    /// 变异：把检查条件从 `!is_terminal()` 改成「任何旧项」⇒ 本用例变红。
    #[test]
    fn dropping_terminal_items_is_allowed() {
        let store = TodoStore::open(None);
        store
            .apply(vec![
                item("已做完", TodoStatus::Completed),
                item("放弃了", TodoStatus::Cancelled),
                item("进行中", TodoStatus::InProgress),
            ])
            .expect("首次写入");

        assert_eq!(
            store.apply(vec![item("进行中", TodoStatus::InProgress)]),
            Ok(ApplyOutcome::Updated)
        );
        assert_eq!(
            store.snapshot(),
            vec![item("进行中", TodoStatus::InProgress)]
        );
    }

    /// 空白 `content` 被拒绝（归一化后为空 ⇒ 渲染出一行没有内容的条目）。
    #[test]
    fn blank_content_is_rejected() {
        let store = TodoStore::open(None);
        assert_eq!(
            store.apply(vec![item("   \n\t ", TodoStatus::Pending)]),
            Err(TodoError::EmptyContent)
        );
        assert!(store.snapshot().is_empty());
    }

    // ── 硬规则 2：无 open 项 ⇒ 清空 + 删文件 ────────────────────────────

    /// 全部终态 ⇒ `ClearedAllDone` + 内存清空 + **文件被删**。
    ///
    /// 变异 1：判据写成「全部 `Completed`」⇒ 含 `Cancelled` 的这一次不再清空 ⇒ 变红。
    /// 变异 2：只清内存不删文件 ⇒ 文件仍在 ⇒ 变红。
    #[test]
    fn all_terminal_clears_and_deletes_file() {
        let dir = test_dir("cleared");
        let store = TodoStore::open(Some(&dir.join("s.jsonl")));
        store
            .apply(vec![
                item("a", TodoStatus::InProgress),
                item("b", TodoStatus::Pending),
            ])
            .expect("首次写入");
        assert!(todo_file(&dir, "s").exists(), "前置：文件已落盘");

        let outcome = store
            .apply(vec![
                item("a", TodoStatus::Completed),
                // 放弃（而非完成）也必须能关闭计划 —— 否则它永远卡住。
                item("b", TodoStatus::Cancelled),
            ])
            .expect("全部终态应被接受");

        assert_eq!(outcome, ApplyOutcome::ClearedAllDone);
        assert!(store.snapshot().is_empty(), "内存必须清空");
        assert!(
            !todo_file(&dir, "s").exists(),
            "文件必须被删除（判据是「无 open 项」，不是「全部 Completed」）"
        );
        assert!(!store.has_open_items());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 单次调用直接提交全终态清单 ⇒ 同样清空（不是只有「先写后关闭」才生效）。
    #[test]
    fn single_shot_all_terminal_clears() {
        let store = TodoStore::open(None);
        assert_eq!(
            store.apply(vec![item("a", TodoStatus::Completed)]),
            Ok(ApplyOutcome::ClearedAllDone)
        );
        assert!(store.snapshot().is_empty());
    }

    /// 仍有 open 项 ⇒ **不**清空，即使其余项已全部终结。
    #[test]
    fn mixed_list_is_kept() {
        let store = TodoStore::open(None);
        assert_eq!(
            store.apply(vec![
                item("a", TodoStatus::Completed),
                item("b", TodoStatus::Pending),
            ]),
            Ok(ApplyOutcome::Updated)
        );
        assert_eq!(store.snapshot().len(), 2);
        assert!(store.has_open_items());
    }

    // ── `has_open_items` 语义 ─────────────────────────────────────────

    /// 全终态 ⇒ `false`；有 `Pending` / `InProgress` ⇒ `true`。
    ///
    /// 变异：把 `InProgress` 当成终态 ⇒ 第二条断言变红（闭环提前结束）。
    #[test]
    fn has_open_items_semantics() {
        let store = TodoStore::open(None);
        assert!(!store.has_open_items(), "空清单 ⇒ 无 open 项");

        store
            .apply(vec![
                item("a", TodoStatus::Completed),
                item("b", TodoStatus::Cancelled),
            ])
            .expect("全终态");
        assert!(!store.has_open_items(), "全终态 ⇒ false");

        store
            .apply(vec![item("c", TodoStatus::Pending)])
            .expect("写 pending");
        assert!(store.has_open_items(), "有 pending ⇒ true");

        store
            .apply(vec![item("c", TodoStatus::InProgress)])
            .expect("改 in_progress");
        assert!(store.has_open_items(), "有 in_progress ⇒ true");

        store
            .apply(vec![item("c", TodoStatus::Cancelled)])
            .expect("改 cancelled");
        assert!(!store.has_open_items(), "改终态 ⇒ 计划被清空 ⇒ false");
        assert!(store.snapshot().is_empty(), "清空是代码做的，不是模型的");
    }

    // ── 落盘 / 恢复 / 损坏降级 ────────────────────────────────────────

    /// 重启恢复：`open(Some(path))` 读回上次写入的内容与状态。
    ///
    /// 变异：`save` 不落盘 / `load` 读错字段 ⇒ 本用例变红。
    #[test]
    fn reopen_restores_previous_content() {
        let dir = test_dir("restore");
        let path = dir.join("s.jsonl");
        let written = vec![
            item("第一步", TodoStatus::Completed),
            item("第二步", TodoStatus::InProgress),
        ];
        {
            let store = TodoStore::open(Some(&path));
            store.apply(written.clone()).expect("写入");
        }

        let reopened = TodoStore::open(Some(&path));
        assert_eq!(reopened.snapshot(), written, "重启后必须读回同样的内容");
        assert!(reopened.has_open_items());
        assert!(
            reopened.take_degraded_hint().is_none(),
            "正常文件不该有降级提示"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件损坏 ⇒ 空列表 + 一句降级提示，**绝不 panic**。
    ///
    /// 变异：把 `load` 的 `Err` 分支改成 `panic!` / `unwrap` ⇒ 本用例直接崩。
    #[test]
    fn corrupt_file_degrades_to_empty_list() {
        let dir = test_dir("corrupt");
        let file = todo_file(&dir, "s");
        fs::write(&file, "{ this is not json").expect("写坏文件");

        let store = TodoStore::open(Some(&dir.join("s.jsonl")));

        assert!(store.snapshot().is_empty(), "坏文件 ⇒ 空列表");
        assert!(!store.has_open_items());
        let hint = store.take_degraded_hint().expect("必须给出降级提示");
        assert!(hint.contains("损坏"), "提示要说清发生了什么：{hint}");
        assert!(
            store.take_degraded_hint().is_none(),
            "降级提示只给一次，避免每次调用重复"
        );

        // 坏文件之后仍可正常写入（不被上一次的损坏挡住）。
        store
            .apply(vec![item("a", TodoStatus::Pending)])
            .expect("坏文件之后仍可写入");
        assert_eq!(store.snapshot(), vec![item("a", TodoStatus::Pending)]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 版本号不认的旧文件 ⇒ 同样退化为空列表 + 提示（不静默误读）。
    #[test]
    fn unknown_version_degrades() {
        let dir = test_dir("version");
        fs::write(
            todo_file(&dir, "s"),
            r#"{"version":99,"items":[{"content":"a","status":"pending"}]}"#,
        )
        .expect("写文件");

        let store = TodoStore::open(Some(&dir.join("s.jsonl")));
        assert!(store.snapshot().is_empty());
        assert!(store.take_degraded_hint().expect("要提示").contains("版本"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空文件 / 不存在的文件 ⇒ 空列表，**且不算降级**（全新会话是正常情况）。
    #[test]
    fn missing_or_empty_file_is_not_degraded() {
        let dir = test_dir("missing");
        let missing = TodoStore::open(Some(&dir.join("nope.jsonl")));
        assert!(missing.snapshot().is_empty());
        assert!(missing.take_degraded_hint().is_none());

        fs::write(todo_file(&dir, "empty"), "   \n").expect("写空文件");
        let empty = TodoStore::open(Some(&dir.join("empty.jsonl")));
        assert!(empty.snapshot().is_empty());
        assert!(empty.take_degraded_hint().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── `/new` retarget ───────────────────────────────────────────────

    /// `retarget` ⇒ 新路径 + 计划为空 + 旧文件被删；**同一 `Arc` 身份不变**。
    ///
    /// 变异：把 `retarget` 写成「新建一个 store」⇒ `Arc::ptr_eq` 断言变红
    /// （工具持的引用会指向旧 store，`/new` 之后写入全进孤儿）。
    #[test]
    fn retarget_switches_file_and_keeps_arc_identity() {
        let dir = test_dir("retarget");
        let old_path = dir.join("old.jsonl");
        let store = TodoStore::open(Some(&old_path));
        let same: Arc<TodoStore> = Arc::clone(&store);

        store
            .apply(vec![item("旧计划", TodoStatus::InProgress)])
            .expect("写入旧会话的计划");
        assert!(todo_file(&dir, "old").exists());

        store.retarget(Some(&dir.join("new.jsonl")));

        assert!(
            Arc::ptr_eq(&store, &same),
            "retarget 必须就地改指向 —— 同一 Arc 身份不变"
        );
        assert!(store.snapshot().is_empty(), "`/new` 后计划归零");
        assert!(!store.has_open_items());
        assert!(!todo_file(&dir, "old").exists(), "旧会话的孤儿文件必须删掉");

        store
            .apply(vec![item("新计划", TodoStatus::Pending)])
            .expect("写入新会话的计划");
        assert!(todo_file(&dir, "new").exists(), "必须写到新文件");
        assert!(!todo_file(&dir, "old").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `retarget(None)` ⇒ 转为纯内存（`-p` 场景不需要，但语义必须自洽）。
    #[test]
    fn retarget_to_none_becomes_memory_only() {
        let dir = test_dir("retarget_none");
        let store = TodoStore::open(Some(&dir.join("s.jsonl")));
        store
            .apply(vec![item("a", TodoStatus::Pending)])
            .expect("写入");

        store.retarget(None);

        assert!(store.path().is_none());
        assert!(store.snapshot().is_empty());
        assert!(!todo_file(&dir, "s").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 归一化 / 去重 / 版本号 ────────────────────────────────────────

    /// 内容归一化后再比较：只改空白不算「删了一项又加一项」。
    ///
    /// 变异：不归一化 ⇒ 模型把 `"写 代码"` 改成 `"写\n代码"` 就被当成丢弃 ⇒ 变红。
    #[test]
    fn whitespace_only_change_keeps_identity() {
        let store = TodoStore::open(None);
        store
            .apply(vec![item("写  代码", TodoStatus::InProgress)])
            .expect("写入");

        assert_eq!(
            store.apply(vec![item("  写\n代码 ", TodoStatus::Completed)]),
            Ok(ApplyOutcome::ClearedAllDone)
        );
    }

    /// 重复内容按第一次出现算（不让同一项在清单里出现两行）。
    #[test]
    fn duplicate_contents_are_collapsed() {
        let store = TodoStore::open(None);
        store
            .apply(vec![
                item("a", TodoStatus::Pending),
                item("a", TodoStatus::Completed),
            ])
            .expect("写入");

        assert_eq!(store.snapshot(), vec![item("a", TodoStatus::Pending)]);
    }

    /// 版本号：内容真的变了才递增；写入相同内容不递增。
    ///
    /// 不递增是「状态没变就不注入」的前提 —— 版本号空转会让判据每次都认为状态变了。
    #[test]
    fn revision_only_advances_on_real_change() {
        let store = TodoStore::open(None);
        assert_eq!(store.revision(), 0);

        store
            .apply(vec![item("a", TodoStatus::Pending)])
            .expect("首次写入");
        let after_first = store.revision();
        assert_eq!(after_first, 1);

        store
            .apply(vec![item("a", TodoStatus::Pending)])
            .expect("重复写入同样内容");
        assert_eq!(store.revision(), after_first, "同内容不递增版本号");

        store
            .apply(vec![item("a", TodoStatus::Completed)])
            .expect("推进状态");
        assert!(store.revision() > after_first, "内容变化必须递增");
    }

    /// `retarget` 只在**确有内容被清掉**时递增版本号（`/new` 后本来就是空计划时不该动它）。
    ///
    /// 变异：无条件 `revision += 1` ⇒ 本用例变红。
    #[test]
    fn retarget_on_empty_plan_does_not_bump_revision() {
        let dir = test_dir("retarget_rev");
        let store = TodoStore::open(Some(&dir.join("a.jsonl")));
        assert_eq!(store.revision(), 0);

        store.retarget(Some(&dir.join("b.jsonl")));

        assert_eq!(store.revision(), 0, "空计划换指向不该改变内容版本号");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 版本号**不落盘**：重启后从 0 开始（注入判据另有「标记丢失」分支兜底）。
    #[test]
    fn revision_is_not_persisted() {
        let dir = test_dir("revision_disk");
        let path = dir.join("s.jsonl");
        {
            let store = TodoStore::open(Some(&path));
            store
                .apply(vec![item("a", TodoStatus::Pending)])
                .expect("写入");
            assert!(store.revision() > 0);
        }

        let reopened = TodoStore::open(Some(&path));
        assert_eq!(reopened.revision(), 0, "版本号不落盘");
        assert_eq!(reopened.snapshot().len(), 1, "但内容在");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
