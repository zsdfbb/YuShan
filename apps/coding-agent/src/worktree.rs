//! git worktree 的**唯一调用点**（全仓仅此模块 spawn `git`）。
//!
//! # 职责
//!
//! 把「一个需求 = 一棵 git 工作树」安置在 ys 自己的目录下（[`worktrees_dir`]），
//! 供 CLI 四动词 `create` / `attach` / `list` / `remove` 调用：
//!
//! - [`create`]：`git worktree add -b worktree-<name> <落点> HEAD`（**只建树**，不建软链）；
//! - [`list`]：解析 `git worktree list --porcelain`，**只保留落在本仓落点子树下的
//!   条目**（那是我们管的工作树）；
//! - [`remove`]：直套 `git worktree remove`（**天然留分支、脏则拒删**）；
//! - [`attach_target`]：把会话工作区定位到某棵已存在的树上（唯一的 ys 特有动作）。
//!
//! # 落点与主仓根
//!
//! 落点是 `[worktrees_dir] / <仓库的 HOME 相对路径，逐分量作目录> / <name>`：仓库在
//! HOME 下时取相对路径（`~/Develop/YuShan` + `feat` ⇒ `Develop/YuShan/feat/`），
//! HOME 外则用绝对路径的各分量（去掉根分隔符）。逐分量**嵌套**（而非拼成单个目录名）
//! 使 `~/a/b` 与 `~/a-b` 不会撞同一落点、`list` 也不互相串台。产物落在仓库**之外**，
//! 不污染主树 `git status`、不被 `grep -r` 搜进完整副本。
//!
//! 主仓根取 `git rev-parse --git-common-dir` 的**父目录**，**不是** `--show-toplevel`
//! —— 后者在 linked worktree 内运行会返回**树根**，据此派生的落点会多一层错名。
//!
//! # 错误
//!
//! [`WorktreeError`] 区分「环境不满足」（[`WorktreeError::NotARepo`] /
//! [`WorktreeError::GitUnavailable`]）与「调用失败」（[`WorktreeError::Git`]）：
//! spawn 失败（未装 git）走前者，非零退出走后者并带上 git 的 stderr。

use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// worktree 操作错误。
///
/// `Display` 为中文文案，带足以定位问题的上下文（git 的 stderr、非法名、退化路径）；
/// 括号内保留一个稳定标识（`NotARepo` / `GitUnavailable` / …），供脚本与测试断言。
#[derive(Debug)]
pub enum WorktreeError {
    /// 当前目录不在 git 仓库内（或 `rev-parse` 非零退出）。
    NotARepo,
    /// 找不到 `git` 可执行文件（spawn 失败）。
    GitUnavailable,
    /// `name` 非法（空、含路径分隔符、`. ` / `..`）。
    InvalidName(String),
    /// 指定的工作树不存在。
    NotFound(String),
    /// 未指名 `attach` 目标、且**无法自动选定**（本仓 0 棵或多棵工作树）。
    ///
    /// `Display` 即给用户的完整提示（含候选名字与路径）。刻意**不**复用 [`Self::NotFound`]：
    /// 后者的外壳会把提示塞进反引号、并拼出无意义的 `create <提示>` 退路。
    NoAttachTarget(String),
    /// git 命令非零退出（附 stderr 原因）。
    Git(String),
}

impl std::fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorktreeError::NotARepo => write!(
                f,
                "当前目录不是 git 仓库（NotARepo）；worktree 只能在 git 仓库内使用"
            ),
            WorktreeError::GitUnavailable => write!(
                f,
                "找不到 git 可执行文件（GitUnavailable）；请先安装 git 并确保它在 PATH 中"
            ),
            WorktreeError::InvalidName(name) => write!(
                f,
                "非法的 worktree 名 `{name}`（InvalidName）：不得为空、不得含 `/` 或 `\\`，也不得是 `.` 或 `..`"
            ),
            WorktreeError::NotFound(name) => write!(
                f,
                "未找到 worktree `{name}`（NotFound）；可先 `ys-coding-agent worktree create {name}`"
            ),
            WorktreeError::NoAttachTarget(msg) => write!(f, "{msg}"),
            WorktreeError::Git(msg) => write!(f, "git 命令失败（Git）：{msg}"),
        }
    }
}

impl std::error::Error for WorktreeError {}

/// 主仓信息：仓库根（`--git-common-dir` 的父目录）与其目录名。
#[derive(Debug, Clone)]
pub struct RepoInfo {
    pub root: PathBuf,
    pub name: String,
}

/// 一棵受管工作树的记录。
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub dirty: bool,
}

/// 受管工作树的父目录：`YUSHAN_WORKTREES_DIR` 覆盖，否则 `$HOME/.yushan/worktrees`。
///
/// `HOME` / `USERPROFILE` 缺失或非绝对 ⇒ 回退到**绝对**的临时目录（照 `sessions_dir`）。
/// 绝不能回退成相对路径：相对路径会解析到 **CWD**（用户仓库），把树建进仓库内、污染
/// `git status`。
pub fn worktrees_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("YUSHAN_WORKTREES_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(home) = home_dir()
        && home.is_absolute()
    {
        return home.join(".yushan").join("worktrees");
    }
    // `TMPDIR` 被设成相对路径时 `temp_dir()` 会原样返回它 —— 需要一个恒为绝对的兜底
    // （本项目只支持 Linux/macOS）。
    let base = std::env::temp_dir();
    let base = if base.is_absolute() {
        base
    } else {
        PathBuf::from("/tmp")
    };
    base.join(format!("yushan-worktrees-{}", std::process::id()))
}

/// `$HOME`（回退 `$USERPROFILE`，与 `config::Config` 的解析保持一致）。
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// 取规范化路径；路径不存在时原样返回（不报错）。
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// 校验 `name`：非空、不含 `/` 或 `\`、不等于 `.` / `..`。
///
/// 校验是**必须**的：`path_for` 直接把它拼进路径，未校验的 `../x` 会把落点带出
/// ys 目录，使 `create` 在任意位置建树。
pub fn validate_name(name: &str) -> Result<(), WorktreeError> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return Err(WorktreeError::InvalidName(name.to_string()));
    }
    Ok(())
}

/// 按「仓库的 HOME 相对路径」计算工作树落点（`<dir>/<分量…>/<name>`）。
///
/// 仓库在 HOME 下 ⇒ 用相对路径的各分量逐级作目录（`~/Develop/YuShan` + `feat`
/// ⇒ `<dir>/Develop/YuShan/feat`）；在 HOME 外（或就是 HOME）⇒ 用绝对路径的各分量
/// （去掉根分隔符）。**逐分量嵌套**而非拼成单个目录名，故 `~/a/b` 与 `~/a-b` 落点不同。
pub fn path_for(repo: &Path, name: &str) -> Result<PathBuf, WorktreeError> {
    validate_name(name)?;
    let mut path = worktrees_dir();
    for comp in repo_components(repo) {
        path.push(comp);
    }
    path.push(name);
    Ok(path)
}

/// 仓库路径的落点分量（见 [`path_for`]）：HOME 相对路径的各分量；HOME 外 / 就是 HOME
/// ⇒ 绝对路径的各分量（丢弃根等非普通分量）。
fn repo_components(repo: &Path) -> Vec<String> {
    let repo = canonical(repo);
    let relative = home_dir()
        .map(|home| canonical(&home))
        .and_then(|home| repo.strip_prefix(&home).ok().map(Path::to_path_buf))
        .filter(|rel| !rel.as_os_str().is_empty());
    let basis = relative.as_deref().unwrap_or(repo.as_path());
    basis
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

/// 自动命名：复用既有会话 id 的形态 `{unix 秒}_{纳秒零填充}`（`wiring::new_session_id`）。
///
/// 零新依赖、与既有 id 同风格；`attach` 可列表选取 ⇒ 名字不必好记。
///
/// 进程内**单调递增**：`SystemTime` 的实际精度在 macOS 只到微秒，同一时刻连续两次
/// 调用会取到同一个值，故用一个全局原子量兜底（相同则 +1），保证同进程内不撞名。
pub fn auto_name() -> String {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let now = now
        .as_secs()
        .saturating_mul(1_000_000_000)
        .saturating_add(now.subsec_nanos() as u64);
    let key = loop {
        let last = LAST.load(Ordering::Relaxed);
        let next = if now > last {
            now
        } else {
            last.saturating_add(1)
        };
        if LAST
            .compare_exchange_weak(last, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            break next;
        }
    };
    format!("{}_{:09}", key / 1_000_000_000, key % 1_000_000_000)
}

/// 主仓根与名：`git rev-parse --git-common-dir` 的父目录（**不是** `--show-toplevel`）。
pub fn main_repo(cwd: &Path) -> Result<RepoInfo, WorktreeError> {
    let out = git(cwd, &["rev-parse", "--git-common-dir"])?;
    if !out.ok {
        return Err(WorktreeError::NotARepo);
    }
    let raw = out.stdout.trim();
    if raw.is_empty() {
        return Err(WorktreeError::NotARepo);
    }
    // `--git-common-dir` 在仓根给出 `.git`、在子目录给出 `../.git` ⇒ 相对值须对 cwd 解析。
    let common = PathBuf::from(raw);
    let common = if common.is_absolute() {
        common
    } else {
        cwd.join(common)
    };
    let root = canonical(&common)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .ok_or(WorktreeError::NotARepo)?;
    let name = root
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .ok_or(WorktreeError::NotARepo)?;
    Ok(RepoInfo { root, name })
}

/// 建一棵工作树：`name` 为 `None` 时自动命名。
///
/// 只做 `git worktree add`；**不建任何软链**。项目级 `.yushan/`（skills / SYSTEM.md）是未跟踪
/// 文件，不随 `git worktree` 迁入 ⇒ 树内不可见，由 `attach` 时的缺失警告告知用户（`main.rs`）。
///
/// 失败提示**区分两类**（[`create_error_hint`]）：同名分支 / 落点已存在 ⇒ 建议换名；
/// 陈旧登记（用户 `rm -rf` 过树）⇒ 先 `git worktree prune` 再重试**一次**。
pub fn create(repo: &Path, name: Option<&str>) -> Result<Entry, WorktreeError> {
    let name = match name {
        Some(n) => {
            validate_name(n)?;
            n.to_string()
        }
        None => auto_name(),
    };
    let path = path_for(repo, &name)?;
    let root = canonical(repo);
    let branch = format!("worktree-{name}");
    let path_str = path.to_string_lossy().into_owned();

    // 落点父目录（worktrees 根到该仓子树）先建好，git 才好在其中建树。
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut out = git(
        &root,
        &["worktree", "add", "-b", &branch, &path_str, "HEAD"],
    )?;
    if !out.ok {
        // 仅当落点**已被登记为工作树、但其目录不存在**（用户 `rm -rf` 过树）才 prune 并
        // 重试一次；分支 / 落点与既有条目冲突时 prune 无效（还可能误清别的登记），直接报错。
        if is_stale_registration(&root, &path) {
            let _ = git(&root, &["worktree", "prune"]);
            out = git(
                &root,
                &["worktree", "add", "-b", &branch, &path_str, "HEAD"],
            )?;
        }
        if !out.ok {
            let msg = reason(&out);
            return Err(WorktreeError::Git(format!(
                "{msg}；{}",
                create_error_hint(&root, &name, &branch, &path)
            )));
        }
    }

    Ok(Entry {
        name,
        path,
        branch: Some(branch),
        dirty: false,
    })
}

/// `create` 的失败是否为**陈旧登记**：git 仍把落点登记为工作树，但其目录不存在
/// （用户 `rm -rf` 过树）。
///
/// 按**状态**判定（登记在、目录不在）而非匹配 git 的 stderr 文案 —— 文案会随版本 /
/// locale 变化。只有这一类值得 `prune` 后重试；分支 / 落点与既有条目冲突时 prune 无效
/// （还可能误清别的登记），不重试。
fn is_stale_registration(root: &Path, path: &Path) -> bool {
    if path.exists() {
        return false;
    }
    git(root, &["worktree", "list", "--porcelain"])
        .map(|out| {
            out.stdout
                .lines()
                .filter_map(|line| line.strip_prefix("worktree "))
                .any(|p| canonical(Path::new(p.trim())) == canonical(path))
        })
        .unwrap_or(false)
}

/// `create` 失败的**下一步**提示：区分「名字冲突（换名）」与「陈旧登记（prune）」。
///
/// 冲突类型按**状态**判定（分支是否已存在、落点是否已占用），**不匹配 git 的 stderr
/// 文案** —— 与 [`is_stale_registration`] 同一条原则。git 的报错会随版本与 locale 变：
/// 同一个失败在 `LANG=zh_CN.UTF-8` 下是「一个名为 'worktree-feat' 的分支已经存在」，
/// 按 `contains("a branch named")` 判就会落进兜底提示，而用户真正需要的「换名」建议
/// 反而丢了。
fn create_error_hint(root: &Path, name: &str, branch: &str, path: &Path) -> String {
    // 先判分支：`worktree add -b` 先碰分支，分支冲突时落点根本没被创建。
    if branch_exists(root, branch) {
        format!("分支 `{branch}` 已存在；请换一个名字")
    } else if path.exists() {
        format!("落点已存在同名条目；请换一个名字，或先清理 `{name}` 的既有目录")
    } else {
        "若提示该路径已被登记，可先执行 `git worktree prune` 后重试".to_string()
    }
}

/// 分支是否已存在（`git show-ref --verify` 的退出码，不解析输出）。
fn branch_exists(root: &Path, branch: &str) -> bool {
    let refname = format!("refs/heads/{branch}");
    git(root, &["show-ref", "--verify", "--quiet", &refname])
        .map(|out| out.ok)
        .unwrap_or(false)
}

/// 列出落在**本仓落点子树**下的工作树（含路径 / 分支 / 是否脏）。
pub fn list(repo: &Path) -> Result<Vec<Entry>, WorktreeError> {
    let root = canonical(repo);
    let out = git(&root, &["worktree", "list", "--porcelain"])?;
    if !out.ok {
        return Err(WorktreeError::Git(reason(&out)));
    }
    let mut base = canonical(&worktrees_dir());
    for comp in repo_components(&root) {
        base.push(comp);
    }

    let mut entries = Vec::new();
    for block in out.stdout.split("\n\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        let mut raw_path: Option<PathBuf> = None;
        let mut branch: Option<String> = None;
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                raw_path = Some(PathBuf::from(p.trim()));
            } else if let Some(b) = line.strip_prefix("branch ") {
                let b = b.trim();
                branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
            }
        }
        let Some(raw_path) = raw_path else { continue };
        let path = canonical(&raw_path);
        // 只保留本仓子树下的**直接子目录**（我们的落点形状 `<base>/<name>`）；
        // 用 parent 精确比对，避免不同仓（如 `~/a/b` 与 `~/a-b`）互相串台。
        if path.parent() != Some(base.as_path()) {
            continue;
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dirty = is_dirty(&path);
        entries.push(Entry {
            name,
            path,
            branch,
            dirty,
        });
    }
    Ok(entries)
}

/// 删一棵工作树：直套 `git worktree remove`（**留分支、脏则拒删**）。
///
/// 失败时把 git 的 stderr 原因原样透传（如「有未提交改动」）。**绝不**自动加任何强制选项
/// —— 那会把真实改动一并删掉；是否强制丢弃由**用户**在 `main.rs` 的退路提示后自行决定。
pub fn remove(repo: &Path, name: &str) -> Result<(), WorktreeError> {
    validate_name(name)?;
    let path = path_for(repo, name)?;
    let root = canonical(repo);
    let path_str = path.to_string_lossy().into_owned();
    let out = git(&root, &["worktree", "remove", &path_str])?;
    if !out.ok {
        return Err(WorktreeError::Git(reason(&out)));
    }
    Ok(())
}

/// 定位 `attach` 的目标：返回已存在的工作树路径。
///
/// 非 git / git 不可用 ⇒ 相应错误；树不存在 ⇒ [`WorktreeError::NotFound`]。
pub fn attach_target(cwd: &Path, name: &str) -> Result<PathBuf, WorktreeError> {
    validate_name(name)?;
    let repo = main_repo(cwd)?;
    let path = path_for(&repo.root, name)?;
    if !path.is_dir() {
        return Err(WorktreeError::NotFound(name.to_string()));
    }
    Ok(path)
}

/// 主仓是否存在项目级配置目录 `<主仓根>/.yushan`（**目录存在即可**）。
///
/// 项目级 `.yushan/`（skills / `.yushan/SYSTEM.md`）是未跟踪文件，不随 `git worktree` 迁入
/// ⇒ 树内不可见。`main.rs` 在 `attach` 成功后据此向 stderr 打缺失警告（主仓无则不打扰用户）。
pub fn has_project_config(repo_root: &Path) -> bool {
    repo_root.join(".yushan").is_dir()
}

/// `git -C <dir> <args>` 的输出快照。
struct GitOutput {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// spawn `git`。spawn 失败（未装 git）⇒ [`WorktreeError::GitUnavailable`]。
///
/// 隔离父进程 env：`GIT_DIR` / `GIT_WORK_TREE` 若被父进程导出，会覆盖 `-C <dir>` 的
/// 定位，把命令指到别处 —— 显式移除，保证只认 `-C`。
fn git(dir: &Path, args: &[&str]) -> Result<GitOutput, WorktreeError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .map_err(|_| WorktreeError::GitUnavailable)?;
    Ok(GitOutput {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// 提取失败原因（优先 stderr，回退 stdout），空则给一句占位。
fn reason(out: &GitOutput) -> String {
    let s = if out.stderr.trim().is_empty() {
        out.stdout.trim()
    } else {
        out.stderr.trim()
    };
    if s.is_empty() {
        "（git 未输出原因）".to_string()
    } else {
        s.to_string()
    }
}

/// 树内是否有未提交改动（`git status --porcelain` 判空）。失败视为「不脏」。
fn is_dirty(path: &Path) -> bool {
    git(path, &["status", "--porcelain"])
        .map(|out| out.ok && !out.stdout.trim().is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::{EnvRestore, env_lock};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 进程内唯一临时目录（原子序号 + `process::id()`，与 `main.rs` / `toolset.rs` 同规）。
    fn unique_tmp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "yushan_worktree_{tag}_{}_{}",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `auto_name` 形如 `{secs}_{9 位纳秒}`，且连续两次不同。
    #[test]
    fn auto_name_is_unique_and_well_formed() {
        let a = auto_name();
        let b = auto_name();
        assert_ne!(a, b, "两次自动命名不应相同：{a} / {b}");

        let (secs, nanos) = a.split_once('_').expect("应含下划线分隔");
        assert!(secs.chars().all(|c| c.is_ascii_digit()), "秒应为数字：{a}");
        assert_eq!(nanos.len(), 9, "纳秒应为 9 位零填充：{a}");
        assert!(
            nanos.chars().all(|c| c.is_ascii_digit()),
            "纳秒应为数字：{a}"
        );
    }

    /// `validate_name` 拒绝空 / 路径分隔符 / `.` / `..`，接受普通名。
    #[test]
    fn validate_name_rejects_unsafe_and_accepts_simple() {
        for bad in ["", "a/b", "..", "a\\b", "."] {
            assert!(
                matches!(validate_name(bad), Err(WorktreeError::InvalidName(_))),
                "应拒绝：{bad:?}"
            );
        }
        assert!(validate_name("feat").is_ok());
    }

    /// `path_for`：HOME 下用相对路径的**各分量**作目录；HOME 外用绝对路径分量；
    /// `~/a/b` 与 `~/a-b` 不得撞同一落点（消除前缀歧义）。
    #[test]
    fn path_for_nests_home_relative_components() {
        let _guard = env_lock();
        let _restore = EnvRestore::capture(&["HOME", "USERPROFILE", "YUSHAN_WORKTREES_DIR"]);

        let home = unique_tmp_dir("home");
        let trees = unique_tmp_dir("trees");
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::remove_var("USERPROFILE");
            std::env::set_var("YUSHAN_WORKTREES_DIR", &trees);
        }

        // 仓库在 HOME 下 ⇒ 各分量逐级作目录。
        let inside = home.join("Develop").join("YuShan");
        std::fs::create_dir_all(&inside).unwrap();
        let p = path_for(&inside, "feat").unwrap();
        assert_eq!(
            p,
            trees.join("Develop").join("YuShan").join("feat"),
            "HOME 相对路径应逐分量嵌套"
        );

        // 前缀歧义：`~/a/b` 与 `~/a-b` 落点必须不同。
        let a_b = home.join("a").join("b");
        let a_b_dash = home.join("a-b");
        std::fs::create_dir_all(&a_b).unwrap();
        std::fs::create_dir_all(&a_b_dash).unwrap();
        assert_ne!(
            path_for(&a_b, "feat").unwrap(),
            path_for(&a_b_dash, "feat").unwrap(),
            "`a/b` 与 `a-b` 不得撞同一落点"
        );

        // 仓库在 HOME 外 ⇒ 绝对路径的各分量嵌套（去掉根分隔符）。
        let outside = unique_tmp_dir("outside");
        let p = path_for(&outside, "feat").unwrap();
        assert!(p.starts_with(&trees), "落点应在 worktrees 根下：{p:?}");
        assert_eq!(p.file_name().unwrap(), "feat");
        let outside_c = canonical(&outside);
        let rel = outside_c.strip_prefix("/").unwrap();
        assert!(
            p.parent().unwrap().ends_with(rel),
            "绝对路径分量应原样嵌套：{p:?}"
        );

        // 非法名仍被拒（先于任何拼接）。
        assert!(matches!(
            path_for(&inside, "../evil"),
            Err(WorktreeError::InvalidName(_))
        ));
    }

    /// `worktrees_dir`：无 `HOME` / `USERPROFILE` ⇒ 回退到**绝对**路径
    /// （相对路径会把树建进仓库内）。
    #[test]
    fn worktrees_dir_falls_back_to_absolute_without_home() {
        let _guard = env_lock();
        let _restore = EnvRestore::capture(&["HOME", "USERPROFILE", "YUSHAN_WORKTREES_DIR"]);
        unsafe {
            std::env::remove_var("HOME");
            std::env::remove_var("USERPROFILE");
            std::env::remove_var("YUSHAN_WORKTREES_DIR");
        }
        let dir = worktrees_dir();
        assert!(
            dir.is_absolute(),
            "无 HOME 时的回退必须是绝对路径（否则树会落进仓库内）：{dir:?}"
        );
    }

    /// `attach_target`（**N7**）：仓内已建同名树 ⇒ 返回该树路径；树不存在 ⇒ `NotFound`；
    /// 非 git ⇒ `NotARepo`。
    #[test]
    fn attach_target_resolves_existing_tree_and_reports_missing() {
        let _guard = env_lock();
        let _restore = EnvRestore::capture(&["HOME", "USERPROFILE", "YUSHAN_WORKTREES_DIR"]);

        let home = unique_tmp_dir("attach_home");
        let trees = unique_tmp_dir("attach_trees");
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::remove_var("USERPROFILE");
            std::env::set_var("YUSHAN_WORKTREES_DIR", &trees);
        }

        // 真建一个 git 仓：`attach_target` 先走 `main_repo`（`rev-parse --git-common-dir`）。
        let repo = home.join("Develop").join("YuShan");
        std::fs::create_dir_all(&repo).unwrap();
        let init = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q"])
            .output()
            .expect("spawn git（测试环境应装有 git）");
        assert!(init.status.success(), "git init 应成功");

        // 成功：落点上已存在同名目录（`attach_target` 只认 `is_dir`）。
        let want = path_for(&repo, "feat").unwrap();
        std::fs::create_dir_all(&want).unwrap();
        assert_eq!(
            attach_target(&repo, "feat").unwrap(),
            want,
            "应返回既有树路径"
        );

        // 失败：树不存在。
        assert!(
            matches!(
                attach_target(&repo, "ghost"),
                Err(WorktreeError::NotFound(_))
            ),
            "不存在的树应报 NotFound"
        );

        // 失败：非 git 目录。
        let plain = unique_tmp_dir("attach_plain");
        assert!(
            matches!(attach_target(&plain, "feat"), Err(WorktreeError::NotARepo)),
            "非 git 目录应报 NotARepo"
        );
    }

    /// `is_stale_registration` —— `create` 的 **prune 重试判据**：git 仍把落点登记为工作树、
    /// 但其目录已被删（用户 `rm -rf`）⇒ `true`；目录在 / 未登记 / 已 prune ⇒ `false`。
    ///
    /// 直接覆盖谓词的原因：本机 git 下「rm -rf 后同名 `create` 经 prune 重试而**成功**」这条
    /// 路径**不可达** —— 失败的首个 `worktree add -b` 会留下同名分支（实测），而 `prune` 不删
    /// 分支 ⇒ 重试必撞「分支已存在」。故成功的重试无集成用例，此处把谓词（及其随 prune 的翻转）
    /// 钉死；集成侧另用 `tests/worktree_cli.rs` 断言 prune 确实执行。
    #[test]
    fn is_stale_registration_tracks_registered_but_missing_dir() {
        let _guard = env_lock();
        let _restore = EnvRestore::capture(&["HOME", "USERPROFILE", "YUSHAN_WORKTREES_DIR"]);

        // 目录取 **canonical**：`temp_dir()` 派生 `/var/…`，而 git 报告 `/private/var/…`；
        // 对**已删除**的路径 `canonicalize` 不再解析符号链接 ⇒ 两侧字面不等，谓词会误判。
        let home = canonical(&unique_tmp_dir("stale_home"));
        let trees = canonical(&unique_tmp_dir("stale_trees"));
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::remove_var("USERPROFILE");
            std::env::set_var("YUSHAN_WORKTREES_DIR", &trees);
        }

        // 真建一个带一次提交的仓（`worktree add … HEAD` 需要 HEAD 存在）。
        let repo = home.join("Develop").join("YuShan");
        std::fs::create_dir_all(&repo).unwrap();
        let init = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q"])
            .output()
            .expect("spawn git（测试环境应装有 git）");
        assert!(init.status.success(), "git init 应成功");
        let commit = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "-c",
                "user.name=ys-test",
                "-c",
                "user.email=ys-test@example.com",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ])
            .output()
            .unwrap();
        assert!(commit.status.success(), "git commit 应成功");

        let path = path_for(&repo, "feat").unwrap();
        // 尚未登记 ⇒ false。
        assert!(
            !is_stale_registration(&repo, &path),
            "未登记时不应判为陈旧登记：{path:?}"
        );

        // 建树：登记在、目录也在。
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let add = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "add", "-b", "worktree-feat"])
            .arg(&path)
            .arg("HEAD")
            .output()
            .unwrap();
        assert!(
            add.status.success(),
            "worktree add 应成功：{}",
            String::from_utf8_lossy(&add.stderr)
        );
        assert!(path.is_dir(), "树目录应存在：{path:?}");
        assert!(
            !is_stale_registration(&repo, &path),
            "目录在 ⇒ 不是陈旧登记"
        );

        // `rm -rf` 掉目录（登记仍在 git 里）⇒ true。
        std::fs::remove_dir_all(&path).unwrap();
        assert!(!path.exists(), "目录应已删除：{path:?}");
        assert!(
            is_stale_registration(&repo, &path),
            "登记在、目录不在 ⇒ 应判为陈旧登记"
        );

        // `git worktree prune` 清掉登记 ⇒ false（这正是 create 重试前的一步）。
        let prune = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "prune"])
            .output()
            .unwrap();
        assert!(prune.status.success(), "prune 应成功");
        assert!(
            !is_stale_registration(&repo, &path),
            "prune 后登记已清除 ⇒ false"
        );
    }

    /// `create_error_hint` **不看 git 的报错文案**（那随版本 / locale 变）。
    ///
    /// 回归：它曾靠 `msg.contains("a branch named")` 分派，而本机 `LANG=zh_CN.UTF-8`
    /// 下 git 说「一个名为 'worktree-feat' 的分支已经存在」⇒ 两个分支都不匹配 ⇒ 落进
    /// 兵底提示，用户拿不到「换名」这个真正可行的下一步。`create_prunes_stale_…`
    /// 集成用例因此在中文环境下恒定失败。
    ///
    /// 本用例只断言**判据来自状态**：分支已存在 ⇒ 无论 locale、无论 stderr 说了什么，
    /// 都给「换名」。
    ///
    /// 变异：把判据改回匹配文案（`msg.contains(..)`）⇒ `branch_exists` 不再被调用 ⇒ 分支
    /// 已存在时会落进落点分支 / 兵底 ⇒ 变红。
    #[test]
    fn create_error_hint_judges_by_state_not_git_message() {
        let _guard = env_lock();
        let _restore = EnvRestore::capture(&["HOME", "USERPROFILE", "YUSHAN_WORKTREES_DIR"]);

        let home = canonical(&unique_tmp_dir("hint_home"));
        let trees = canonical(&unique_tmp_dir("hint_trees"));
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::remove_var("USERPROFILE");
            std::env::set_var("YUSHAN_WORKTREES_DIR", &trees);
        }

        let repo = home.join("Develop").join("YuShan");
        std::fs::create_dir_all(&repo).unwrap();
        let init = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q"])
            .output()
            .expect("spawn git（测试环境应装有 git）");
        assert!(init.status.success(), "git init 应成功");
        let commit = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "-c",
                "user.name=ys-test",
                "-c",
                "user.email=ys-test@example.com",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ])
            .output()
            .unwrap();
        assert!(commit.status.success(), "git commit 应成功");

        let path = path_for(&repo, "feat").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        // 分支**不存在**、落点也**不存在** ⇒ 两个状态判据都不命中 ⇒ 兵底（prune 建议）。
        let fallback = create_error_hint(&repo, "feat", "worktree-feat", &path);
        assert!(
            fallback.contains("prune"),
            "分支与落点都无冲突 ⇒ 应给 prune 兵底：{fallback}"
        );

        // 建出该分支（不建树）⇒ 只有「分支已存在」这一条命中。
        let create_branch = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["branch", "worktree-feat", "HEAD"])
            .output()
            .unwrap();
        assert!(create_branch.status.success(), "git branch 应成功");

        let by_branch = create_error_hint(&repo, "feat", "worktree-feat", &path);
        assert!(
            by_branch.contains("请换一个名字") && by_branch.contains("分支"),
            "分支已存在 ⇒ 必须建议换名（不得依赖 git 的本地化文案）：{by_branch}"
        );
    }

    /// `WorktreeError` 的 `Display` 非空，且带上可供定位的上下文。
    #[test]
    fn error_display_is_nonempty_and_contextual() {
        let cases = [
            WorktreeError::NotARepo,
            WorktreeError::GitUnavailable,
            WorktreeError::InvalidName("bad/name".into()),
            WorktreeError::NotFound("ghost".into()),
            WorktreeError::NoAttachTarget("本仓还没有工作树".into()),
            WorktreeError::Git("fatal: boom".into()),
        ];
        for e in cases {
            let text = e.to_string();
            assert!(!text.is_empty(), "文案不应为空");
        }
        assert!(
            WorktreeError::InvalidName("bad/name".into())
                .to_string()
                .contains("bad/name")
        );
        assert!(
            WorktreeError::Git("fatal: boom".into())
                .to_string()
                .contains("fatal: boom")
        );
    }
}
