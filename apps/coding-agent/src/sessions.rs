//! 会话目录的只读视图 + 起屏前的选择器。
//!
//! 纯同步、无 TUI 依赖（只依赖 `ys-session` + std）。消费者是 `main`：起屏前
//! 解析 `--resume`（[`resolve`] / [`pick`]），并读回首行 [`SessionMeta`] 决定模型
//! 与显示初值。
//!
//! 只读**首行**且封顶 4 KiB（[`FIRST_LINE_CAP`]）—— meta 行恒远小于此（标题写入前
//! 截断到 500 字符），故与文件大小解耦、内存有界（设计 C4/C6）。读取必须**字节级 +
//! 换行切分 + 后解码**，不能在多字节字符中间切断（流式/字节路径铁律）。

use std::fs::File;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ys_core::{ContentBlock, Message, Role};
use ys_protocol::SessionSummary;
use ys_session::SessionMeta;

/// 只读首行的字节上限。meta 行由 `derive_stored_title` 保证有界，恒落在此内。
const FIRST_LINE_CAP: u64 = 4096;

/// 无 meta / 无标题时的占位显示标题。**缺省标题的唯一定义** ——
/// `capabilities::session_summary` 复用本条，二者不得漂移。
pub(crate) const DEFAULT_TITLE: &str = "新会话";

/// 读文件首行：字节级 + 换行切分 + [`FIRST_LINE_CAP`] 封顶，最后才解码。
///
/// 打不开 / 首行非 UTF-8 / 封顶处切在多字节字符中间（首行超过封顶）⇒ `None`。
fn read_first_line(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(FIRST_LINE_CAP).read_to_end(&mut buf).ok()?;
    let end = buf.iter().position(|&b| b == b'\n').unwrap_or(buf.len());
    String::from_utf8(buf[..end].to_vec()).ok()
}

/// 读回首行 meta（同一封顶实现）。首行不是合法 [`SessionMeta`] ⇒ `None`。
pub fn read_meta(path: &Path) -> Option<SessionMeta> {
    serde_json::from_str::<SessionMeta>(&read_first_line(path)?).ok()
}

/// 列出目录下的会话文件，按 mtime **倒序**（最近活动在前）。
///
/// 目录不存在 ⇒ 空；`*.jsonl` 之外（含 `.jsonl.tmp` 残留）跳过；首行读不出的
/// 文件跳过；无 meta 的文件标题为 [`DEFAULT_TITLE`]。`title` 取自 `meta.title`
/// 折叠截断后的显示形态，其余字段一一对应条目本身。
pub fn list(dir: &Path) -> Vec<SessionSummary> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<SessionSummary> = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        // `path.extension()` 对 `x.jsonl.tmp` 是 `tmp` ⇒ 残留天然被跳过。
        if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let Some(first) = read_first_line(&path) else {
            continue;
        };
        let title = serde_json::from_str::<SessionMeta>(&first)
            .ok()
            .and_then(|m| m.title)
            .map(|t| derive_title(&t))
            .unwrap_or_else(|| DEFAULT_TITLE.to_string());
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        out.push(SessionSummary {
            id,
            path,
            mtime,
            title,
        });
    }
    out.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    out
}

/// 把 `id_or_path` 解析成会话文件路径：先 `dir/{id}.jsonl`，再按字面路径。
/// 两者皆非文件 ⇒ `Err`（起屏前 stderr 报错退出）。
pub fn resolve(dir: &Path, id_or_path: &str) -> Result<PathBuf, String> {
    let by_id = dir.join(format!("{id_or_path}.jsonl"));
    if by_id.is_file() {
        return Ok(by_id);
    }
    let by_path = PathBuf::from(id_or_path);
    if by_path.is_file() {
        return Ok(by_path);
    }
    Err(format!("找不到会话：{id_or_path}"))
}

/// 交互式选择会话（`--resume` 裸形态）。
///
/// 列表与提示一律写 **stderr**（stdout 属数据流，不得污染）；非 TTY ⇒ 打印列表
/// 后 `Err`（调用方提示改用 `--resume <id>`）。
pub fn pick(entries: &[SessionSummary]) -> Result<PathBuf, String> {
    let stdin = std::io::stdin();
    let stdin_is_tty = stdin.is_terminal();
    let mut input = stdin.lock();
    let mut stderr = std::io::stderr();
    pick_with(entries, stdin_is_tty, &mut input, &mut stderr)
}

/// [`pick`] 的内核：一切提示与列表**只**写 `out`（绝无第二个写者）；非 TTY 时
/// 写完列表 + 一行「请用 `--resume <id>`」后返回 `Err`。
///
/// 写者与「是否 TTY」由调用方注入，生产侧 `pick` 传真实 stdin / stderr。
/// 这样测试能断言列表确实落在同一个写者上 —— 否则把 `writeln!` 换成 `println!`
/// （泄漏到 stdout、污染 `-p`/`--json` 的数据流）也不会有人察觉（T4/C8）。
fn pick_with(
    entries: &[SessionSummary],
    stdin_is_tty: bool,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<PathBuf, String> {
    let _ = writeln!(out, "可恢复的会话（时间 = 最近活动，Unix 秒）：");
    for (i, e) in entries.iter().enumerate() {
        let _ = writeln!(
            out,
            "  {:>3}) {:>12}  {}  ({})",
            i + 1,
            unix_secs(e.mtime),
            e.title,
            e.id
        );
    }
    if !stdin_is_tty {
        let _ = writeln!(out, "非交互环境：请用 --resume <id> 指定会话");
        return Err("非 TTY，无法交互选择会话".into());
    }
    let _ = write!(out, "选择编号（回车取消）> ");
    let _ = out.flush();
    let mut line = String::new();
    input
        .read_line(&mut line)
        .map_err(|e| format!("读取选择失败：{e}"))?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err("已取消".into());
    }
    let idx: usize = trimmed
        .parse()
        .map_err(|_| format!("无效编号：{trimmed}"))?;
    if idx == 0 || idx > entries.len() {
        return Err(format!("编号超出范围：{idx}"));
    }
    Ok(entries[idx - 1].path.clone())
}

/// 显示用标题：折叠空白 + `chars().take(50)` + 省略号。
pub fn derive_title(raw: &str) -> String {
    let collapsed = collapse_whitespace(raw);
    let mut out: String = collapsed.chars().take(50).collect();
    if collapsed.chars().count() > 50 {
        out.push('…');
    }
    out
}

/// 落盘用标题：折叠空白 + `chars().take(500)`（保证 meta 行有界，设计 C6）。
pub fn derive_stored_title(raw: &str) -> String {
    collapse_whitespace(raw).chars().take(500).collect()
}

/// 从一条消息取用户原文：仅 `Role::User` 且含非空 `Text` 块时拼接，否则 `None`
/// （纯 `ToolResult` 的 user 消息不算用户文本）。
pub fn user_text(message: &Message) -> Option<String> {
    if message.role != Role::User {
        return None;
    }
    let parts: Vec<&str> = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// 折叠任意空白（含换行）为单个空格并去首尾。
fn collapse_whitespace(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// mtime → Unix 秒（无日期库可用，直接显示整数）。
fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    /// 进程内唯一临时目录（原子序号 + `process::id()`，并行安全）。
    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "yushan_sessions_{tag}_{}_{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn meta_line(title: &str) -> String {
        let mut meta = SessionMeta::new(1);
        meta.title = Some(title.to_string());
        format!("{}\n", serde_json::to_string(&meta).unwrap())
    }

    /// 写文件并显式拨 mtime（`set_modified` 稳定，无需猜写入耗时）。
    fn write_with_mtime(path: &Path, content: &str, mtime: SystemTime) {
        fs::write(path, content).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    /// `list` 按 mtime 倒序，且 title 取自 meta。
    #[test]
    fn list_sorts_by_mtime_desc() {
        let dir = temp_dir("list_order");
        let base = SystemTime::now();
        write_with_mtime(
            &dir.join("old.jsonl"),
            &meta_line("旧"),
            base - Duration::from_secs(100),
        );
        write_with_mtime(&dir.join("new.jsonl"), &meta_line("新"), base);
        write_with_mtime(
            &dir.join("mid.jsonl"),
            &meta_line("中"),
            base - Duration::from_secs(50),
        );

        let entries = list(&dir);
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["new", "mid", "old"], "mtime DESC");
        assert_eq!(entries[0].title, "新");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 只读首行且 4 KiB 封顶（C4）：首行之后塞入 **非法 UTF-8** 的 8 MiB 巨量字节，
    /// `list` 仍能正确读回标题 —— 证明读取在首行（且封顶）处停下：既不越过首行去
    /// 解码（否则撞上非法字节会失败），也不把整文件读进内存。
    #[test]
    fn list_reads_only_first_line_capped() {
        let dir = temp_dir("list_capped");
        let path = dir.join("big.jsonl");
        let mut bytes = meta_line("有界标题").into_bytes();
        bytes.extend_from_slice(&vec![0xFFu8; 8 * 1024 * 1024]); // 8 MiB 非法 UTF-8
        bytes.push(b'\n');
        fs::write(&path, bytes).unwrap();

        let entries = list(&dir);
        assert_eq!(entries.len(), 1, "超大文件不应让读取失败");
        assert_eq!(entries[0].title, "有界标题");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 首行超过封顶（无换行）⇒ 读不出 ⇒ 跳过（不 panic、不 OOM）。
    #[test]
    fn list_skips_file_whose_first_line_exceeds_cap() {
        let dir = temp_dir("list_over_cap");
        let path = dir.join("huge_first.jsonl");
        // 4 KiB+ 无换行；4096 不是 3 的倍数 ⇒ 解码时切在多字节字符中间 ⇒ 跳过。
        let s = "中".repeat(2000);
        fs::write(&path, s.as_bytes()).unwrap();

        assert!(list(&dir).is_empty(), "超封顶首行读不出 ⇒ 跳过");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 不可读的 `*.jsonl`（悬空符号链接）⇒ 跳过；`.jsonl.tmp` 残留 ⇒ 跳过。
    #[test]
    fn list_skips_unreadable_and_tmp_residue() {
        let dir = temp_dir("list_skip");
        write_with_mtime(&dir.join("ok.jsonl"), &meta_line("好"), SystemTime::now());
        fs::write(dir.join("residue.jsonl.tmp"), "junk").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/nonexistent/yushan-probe", dir.join("broken.jsonl")).unwrap();

        let ids: Vec<String> = list(&dir).into_iter().map(|e| e.id).collect();
        assert_eq!(ids, vec!["ok"], "只有真实可读的 .jsonl 入选");

        let _ = fs::remove_dir_all(&dir);
    }

    /// T4：`list` 的契约锚定在 [`SessionSummary`] 上 —— 同一目录里混放真实
    /// `.jsonl`、`.jsonl.tmp` 残留、不可读链接与超封顶首行时，只有前者入选；
    /// 且返回类型**具名**为 `Vec<SessionSummary>`（类型一旦漂移即编译失败）。
    ///
    /// 变异：`list` 放宽扩展名过滤（收 `.jsonl.tmp`）或去掉封顶 ⇒ 本用例变红。
    #[test]
    fn list_contract_pinned_to_session_summary() {
        let dir = temp_dir("list_contract");
        write_with_mtime(
            &dir.join("keep.jsonl"),
            &meta_line("保留"),
            SystemTime::now(),
        );
        fs::write(dir.join("residue.jsonl.tmp"), "junk").unwrap();
        // 首行 4 KiB 以上且无换行 ⇒ 读不出 ⇒ 跳过（封顶语义）。
        fs::write(dir.join("over_cap.jsonl"), "中".repeat(2000).as_bytes()).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/nonexistent/yushan-probe", dir.join("broken.jsonl")).unwrap();

        let entries: Vec<SessionSummary> = list(&dir);
        assert_eq!(entries.len(), 1, "只留真实可读的 .jsonl");
        let only = &entries[0];
        assert_eq!(only.id, "keep");
        assert_eq!(only.path, dir.join("keep.jsonl"));
        assert_eq!(only.title, "保留");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 无 meta 的文件标题缺省为「新会话」；目录不存在 ⇒ 空。
    #[test]
    fn list_defaults_title_and_empty_for_missing_dir() {
        let dir = temp_dir("list_default");
        fs::write(dir.join("plain.jsonl"), "{\"role\":\"user\"}\n").unwrap();
        let entries = list(&dir);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, DEFAULT_TITLE);

        assert!(list(&dir.join("does-not-exist")).is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    /// `resolve`：先 id 后路径；都未命中 ⇒ Err。
    #[test]
    fn resolve_hits_id_then_path_and_errors_when_missing() {
        let dir = temp_dir("resolve");
        let path = dir.join("abc.jsonl");
        fs::write(&path, meta_line("x")).unwrap();

        assert_eq!(resolve(&dir, "abc").unwrap(), path);
        assert_eq!(resolve(&dir, path.to_str().unwrap()).unwrap(), path);
        assert!(resolve(&dir, "nope").is_err());
        assert!(resolve(&dir, "/nonexistent/nope.jsonl").is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    /// 非 TTY ⇒ `pick` 返回 Err（列表已写 stderr）。
    #[test]
    fn pick_errors_when_not_a_tty() {
        let dir = temp_dir("pick");
        let path = dir.join("a.jsonl");
        fs::write(&path, meta_line("标题")).unwrap();
        let entries = list(&dir);
        assert_eq!(entries.len(), 1);

        // cargo test 的 stdin 不是终端 ⇒ 走非 TTY 分支。
        if !std::io::stdin().is_terminal() {
            assert!(pick(&entries).is_err(), "非 TTY 应 Err");
        }

        let _ = fs::remove_dir_all(&dir);
    }

    /// C8/T4：`pick` 的列表与提示**全走注入的写者**（生产侧是 stderr）。非 TTY 时
    /// `pick_with` 返回 `Err`，且表头 / 会话标题 / 会话 id / `请用 --resume <id>`
    /// 提示都落在同一个 `buf` 上。
    ///
    /// 变异：把内核里的 `writeln!(out, …)` 换成 `println!`（写 stdout）⇒ `buf` 缺
    /// 内容 ⇒ 本用例变红。
    #[test]
    fn pick_writes_list_and_hint_to_injected_writer_on_non_tty() {
        let dir = temp_dir("pick_output");
        let path = dir.join("abc.jsonl");
        fs::write(&path, meta_line("我的标题")).unwrap();
        let entries = list(&dir);
        assert_eq!(entries.len(), 1);

        let mut input: &[u8] = b"";
        let mut buf: Vec<u8> = Vec::new();
        let err = pick_with(&entries, false, &mut input, &mut buf).unwrap_err();
        assert!(!err.is_empty(), "非 TTY 应 Err");

        let out = String::from_utf8(buf).expect("输出必须是 UTF-8");
        assert!(out.contains("可恢复的会话"), "缺表头：{out}");
        assert!(out.contains("我的标题"), "缺会话标题：{out}");
        assert!(out.contains("abc"), "缺会话 id：{out}");
        assert!(out.contains("请用 --resume <id>"), "缺提示：{out}");

        let _ = fs::remove_dir_all(&dir);
    }

    /// `derive_title`：折叠空白、50 字符截断加省略号、空文本为空。
    #[test]
    fn derive_title_collapses_truncates_and_handles_empty() {
        assert_eq!(derive_title("  a   b\n\tc  "), "a b c");
        assert_eq!(derive_title(""), "");
        assert_eq!(derive_title("   \n  "), "");

        let long = "字".repeat(60);
        let t = derive_title(&long);
        assert_eq!(t.chars().count(), 51, "50 字符 + 省略号");
        assert!(t.ends_with('…'));

        // 恰好 50 ⇒ 不加省略号。
        let exact = "字".repeat(50);
        assert_eq!(derive_title(&exact).chars().count(), 50);
    }

    /// `derive_stored_title`：折叠空白 + 500 字符截断（无省略号）。
    #[test]
    fn derive_stored_title_truncates_at_500() {
        assert_eq!(derive_stored_title("a \n b"), "a b");
        let long = "字".repeat(600);
        assert_eq!(derive_stored_title(&long).chars().count(), 500);
    }

    /// `user_text`：User 且含非空 Text ⇒ Some；纯 ToolResult / 空白 Text / 非 User ⇒ None。
    #[test]
    fn user_text_skips_non_text_and_non_user() {
        let user_text_msg = Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
        };
        assert_eq!(user_text(&user_text_msg).as_deref(), Some("hello"));

        let tool_result_only = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: ys_core::ToolCallId("c1".into()),
                content: "result".into(),
                is_error: false,
            }],
        };
        assert_eq!(user_text(&tool_result_only), None);

        let blank = Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "   \n".into(),
            }],
        };
        assert_eq!(user_text(&blank), None);

        let assistant = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: "hi".into() }],
        };
        assert_eq!(user_text(&assistant), None);
    }

    /// `read_meta` 只认首行 meta；非 meta 首行 ⇒ None。
    #[test]
    fn read_meta_only_accepts_first_line_meta() {
        let dir = temp_dir("read_meta");
        let good = dir.join("good.jsonl");
        fs::write(&good, meta_line("标题")).unwrap();
        assert_eq!(
            read_meta(&good).and_then(|m| m.title).as_deref(),
            Some("标题")
        );

        let plain = dir.join("plain.jsonl");
        fs::write(&plain, "{\"role\":\"user\"}\n").unwrap();
        assert!(read_meta(&plain).is_none());

        let _ = fs::remove_dir_all(&dir);
    }
}
