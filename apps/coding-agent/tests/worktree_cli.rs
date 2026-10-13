//! **`worktree` 子命令的子进程端到端**：`create` / `list` / `remove` 的进程级动作与
//! `attach` 的降级路径。
//!
//! 用例编号沿用设计期计划 `docs/design-plans/2026-10-11-worktree.md` 的「测试合同」
//! （N10「`.yushan` 软链」已随软链机制取消而**删除**）；当前形态见 `docs/arch/worktree/design.md`。
//! 覆盖：N1（建树 + 分支）/ N2（免起名唯一）/ N3（非 git 硬失败）/ N4（非法名）/ N5（list）/
//! N6（remove 留分支、脏则拒删）/ N8（attach 降级不 panic）/ N9（退出行）/
//! A1（attach 的项目配置缺失警告）/ U3（`--resume` 恢复到记录的工作树，含失效告知），
//! 以及干净树直接 `remove` 的回归
//! [`clean_tree_removes_plainly_but_real_changes_refuse`]。
//!
//! U5 补三条「改错也不会有测试变红」的盲区：[`create_prunes_stale_registration_after_dir_removed`]
//! （陈旧登记的 prune 副作用）、[`main_repo_is_resolved_from_inside_linked_worktree`]
//! （树内取主仓根）、[`no_arg_attach_reports_zero_and_two_candidates`]（无参 attach 的候选提示）。
//!
//! 未在本文件覆盖（各有单测）：N7 `attach_target`
//! （`src/worktree.rs::attach_target_resolves_existing_tree_and_reports_missing`）、
//! N11 `parse_args`（`src/main.rs::parse_worktree_subcommands` /
//! `parse_worktree_attach_modes`）、N12 全量回归（`cargo test --workspace`）。
//!
//! # 子进程 env 隔离
//!
//! 全部经 `Command` 的 **per-child** `.env()` / `.env_remove()` 注入，
//! **绝不改父进程 env**（`crate::test_env` 只在 bin crate 的 `#[cfg(test)]` 里编译，
//! 集成测试用不到；且改写进程级 env 会与同 binary 内并行测试产生数据竞争）。
//!
//! **`PATH` 必须继承**：被测二进制自身要 spawn `git`，若像 `session_cli.rs` 那样把
//! `PATH` 清空，`worktree::git` 会 spawn 失败 ⇒ 全部用例误报 `GitUnavailable`。
//!
//! # 真调 git
//!
//! 本仓此前零 git 调用。临时仓由 [`init_repo`] 用 `git init` + 一次提交建起
//! （`worktree add ... HEAD` 需要一个已存在的 HEAD）；所有 git 命令都带
//! `-c user.name=… -c user.email=…`，否则 commit 会因缺身份而失败。
//!
//! # 挂起即失败
//!
//! 所有用例套 [`CHILD_TIMEOUT`]：挂起必须是 panic（带已捕获输出），而非测试挂死。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// 子进程退出上限。worktree 动作秒级返回；`-p` 回合走回环 mock 也是秒级。
const CHILD_TIMEOUT: Duration = Duration::from_secs(60);

/// 进程内唯一临时目录：**原子序号 + `process::id()`**（并行安全，项目硬约定）。
fn unique_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "yushan_worktree_cli_{tag}_{}_{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 每个用例一套隔离根：临时 HOME / worktrees 根 / 会话目录 / 一个 git 仓 / 一个非 git 目录。
///
/// 仓库刻意放在 `<home>/Develop/YuShan`（HOME 之下）—— 走设计与
/// `path_for` 的主路径（HOME 相对前缀 `Develop/YuShan`），也让 skill 根等
/// 与真实布局一致。
struct Sandbox {
    base: PathBuf,
    home: PathBuf,
    trees: PathBuf,
    sessions: PathBuf,
    /// 临时 git 仓（`<home>/Develop/YuShan`）。
    repo: PathBuf,
    /// 临时**非 git** 目录。
    plain: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        // **base 取 canonical**（解析 `/var`→`/private/var` 等符号链接）：否则 `temp_dir()`
        // 派生的落点是 `/var/…`，而 git 报告的是 `/private/var/…`；对**已删除**的路径，
        // `canonicalize` 不再解析符号链接 ⇒ 两侧字面不等，`is_stale_registration` 会误判为
        // 「非陈旧登记」、prune 不执行（U5-4a 会因此**空过**）。
        let base = canonical(&unique_dir(tag));
        let home = base.join("home");
        let trees = base.join("trees");
        let sessions = base.join("sessions");
        let repo = home.join("Develop").join("YuShan");
        let plain = base.join("plain");
        for d in [&home, &trees, &sessions, &plain] {
            std::fs::create_dir_all(d).unwrap();
        }
        Self {
            base,
            home,
            trees,
            sessions,
            repo,
            plain,
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // best-effort 清理（含 git 建出的树与仓内元数据）。
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// 已套好隔离的被测二进制命令（**per-child env，不碰父进程**）。
///
/// 刻意**不设置 `PATH`**：被测二进制需 spawn `git`（见文件头）。
fn command(sb: &Sandbox, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ys-coding-agent"));
    cmd.current_dir(cwd)
        .env("HOME", &sb.home)
        .env_remove("USERPROFILE")
        .env("YUSHAN_WORKTREES_DIR", &sb.trees)
        .env("YUSHAN_SESSIONS_DIR", &sb.sessions)
        // 模型侧：清干净，使 N8 的「无凭证早退」与 N9 的 mock 凭证都确定。
        .env_remove("YUSHAN_API_BASE")
        .env_remove("YUSHAN_API_KEY")
        .env_remove("YUSHAN_MODEL")
        .env_remove("OPENAI_API_BASE")
        .env_remove("OPENAI_API_KEY")
        .env_remove("YUSHAN_CHANNEL_CAPACITY")
        // 回环 mock：绕过可能存在的代理。
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy");
    cmd
}

/// 在 `cwd` 下跑一次被测二进制（**套 timeout**）。挂起 → panic（带已捕获输出）。
fn run(sb: &Sandbox, cwd: &Path, argv: &[&str]) -> Output {
    let mut cmd = command(sb, cwd);
    cmd.args(argv);
    run_cmd(cmd, argv)
}

fn run_cmd(mut cmd: Command, argv: &[&str]) -> Output {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(cmd.output());
    });
    match rx.recv_timeout(CHILD_TIMEOUT) {
        Ok(out) => out.expect("spawn ys-coding-agent"),
        Err(_) => panic!(
            "ys-coding-agent {argv:?} 在 {CHILD_TIMEOUT:?} 内未退出 = 挂起\
             （必须表现为失败，而非测试挂死）"
        ),
    }
}

/// stderr / stdout 的 lossy 解码（子进程输出可能含非 UTF-8，绝不 panic）。
fn streams(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// 取以某个前缀开头的行（去前缀）。用于从 stderr 抓 `worktree: <name>` / `路径：<path>`。
fn line_after<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.lines().find_map(|l| l.strip_prefix(prefix))
}

/// `create` 成功时打印的树名（`worktree: <name>`）。
fn created_name(stderr: &str) -> String {
    line_after(stderr, "worktree: ")
        .expect("create 成功应打印 `worktree: <name>`")
        .trim()
        .to_string()
}

/// `create` 成功时打印的树路径（`路径：<path>`）。
fn created_path(stderr: &str) -> PathBuf {
    PathBuf::from(
        line_after(stderr, "路径：")
            .expect("create 成功应打印 `路径：<path>`")
            .trim(),
    )
}

/// 真调 `git`：`-C <dir>` + 身份 `-c`（否则 commit 因缺身份失败）+ `args`。
///
/// 身份用 `-c` 而非 `Command::env`：`-c` 对所有 `git` 子命令生效，无遗漏。
fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=ys-test", "-c", "user.email=ys-test@example.com"])
        .args(args)
        .output()
        .expect("spawn git（测试环境应装有 git）")
}

/// 建一个带**一次提交**的临时 git 仓（`worktree add ... HEAD` 需要 HEAD 存在）。
fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let init = git(dir, &["init", "-q"]);
    assert!(
        init.status.success(),
        "git init 应成功：{}",
        String::from_utf8_lossy(&init.stderr)
    );
    std::fs::write(dir.join("README.md"), "init\n").unwrap();
    let add = git(dir, &["add", "-A"]);
    assert!(add.status.success(), "git add 应成功");
    let commit = git(dir, &["commit", "-q", "-m", "init"]);
    assert!(
        commit.status.success(),
        "git commit 应成功：{}",
        String::from_utf8_lossy(&commit.stderr)
    );
}

/// 被测二进制在 `cwd` 里跑 `worktree create <name>` 并返回（树路径，stderr）。
fn create(sb: &Sandbox, name: &str) -> (PathBuf, String) {
    let out = run(sb, &sb.repo, &["worktree", "create", name]);
    let (_, stderr) = streams(&out);
    assert_eq!(out.status.code(), Some(0), "create {name} 应成功；stderr:\n{stderr}");
    (created_path(&stderr), stderr)
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

// ---------------------------------------------------------------------------
// N1 —— `create <name>` 建树 + 建分支
// ---------------------------------------------------------------------------

/// **N1**：临时 git 仓内 `worktree create feat`。
///
/// 断言：退出码 0；stdout 为空（create 的输出全走 stderr）；stderr 含 `worktree: feat`；
/// `git worktree list` 含新路径；`git branch --list worktree-feat` 非空。
///
/// 变异靶子：把 `worktree_exit_line` 的 `eprintln!` 改成 `println!` ⇒ stdout 非空 ⇒ 红；
/// 分支名少 `worktree-` 前缀 ⇒ 分支断言红。
#[test]
fn n1_create_makes_worktree_and_branch() {
    let sb = Sandbox::new("n1");
    init_repo(&sb.repo);

    let out = run(&sb, &sb.repo, &["worktree", "create", "feat"]);
    let (stdout, stderr) = streams(&out);

    assert_eq!(out.status.code(), Some(0), "create 应 exit(0)；stderr:\n{stderr}");
    assert!(
        stdout.is_empty(),
        "create 的输出属于诊断，应全走 stderr；stdout:\n{stdout:?}"
    );
    assert!(stderr.contains("worktree: feat"), "stderr 应含 `worktree: feat`：\n{stderr}");

    let tree = created_path(&stderr);
    assert!(tree.is_dir(), "打印的树路径应是目录：{tree:?}");
    assert!(
        tree.starts_with(&sb.trees),
        "树应落在 YUSHAN_WORKTREES_DIR 下：{tree:?}"
    );

    // `git worktree list` 含新路径（按 canonical 比对，避免 temp 路径的符号链接差异）。
    let list = git(&sb.repo, &["worktree", "list", "--porcelain"]);
    let listed = String::from_utf8_lossy(&list.stdout).into_owned();
    let want = canonical(&tree);
    let found = listed
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .any(|p| canonical(Path::new(p.trim())) == want);
    assert!(found, "git worktree list 应含新树路径 {want:?}；实际：\n{listed}");

    let branch = git(&sb.repo, &["branch", "--list", "worktree-feat"]);
    assert!(
        String::from_utf8_lossy(&branch.stdout).contains("worktree-feat"),
        "应建出分支 worktree-feat"
    );
}

// ---------------------------------------------------------------------------
// N2 —— `create` 免起名（自动命名唯一）
// ---------------------------------------------------------------------------

/// **N2**：连跑两次 `worktree create`（不带名）。
///
/// 断言：两次都 exit 0；stderr 打的树名非空；两次名字**不同**；对应目录都存在。
#[test]
fn n2_create_without_name_autonames_uniquely() {
    let sb = Sandbox::new("n2");
    init_repo(&sb.repo);

    let mut names = Vec::new();
    let mut paths = Vec::new();
    for _ in 0..2 {
        let out = run(&sb, &sb.repo, &["worktree", "create"]);
        let (_, stderr) = streams(&out);
        assert_eq!(out.status.code(), Some(0), "自动命名 create 应成功；stderr:\n{stderr}");
        let name = created_name(&stderr);
        assert!(!name.is_empty(), "自动命名不应为空：\n{stderr}");
        let path = created_path(&stderr);
        assert!(path.is_dir(), "自动命名的树目录应存在：{path:?}");
        names.push(name);
        paths.push(path);
    }

    assert_ne!(names[0], names[1], "两次自动命名不应相同：{names:?}");
    assert_ne!(paths[0], paths[1], "两次落点不应相同：{paths:?}");
}

// ---------------------------------------------------------------------------
// N3 —— 非 git 目录硬失败
// ---------------------------------------------------------------------------

/// **N3**：在非 git 目录跑 `worktree create x` ⇒ 退出码非 0、stderr 含 `NotARepo`。
#[test]
fn n3_create_outside_repo_hard_fails() {
    let sb = Sandbox::new("n3");

    let out = run(&sb, &sb.plain, &["worktree", "create", "x"]);
    let (stdout, stderr) = streams(&out);

    assert_ne!(out.status.code(), Some(0), "非 git 目录应硬失败；stderr:\n{stderr}");
    assert!(
        stderr.contains("NotARepo"),
        "stderr 应给出明确原因（NotARepo）：\n{stderr}"
    );
    assert!(stdout.is_empty(), "失败路径不写 stdout：{stdout:?}");
}

// ---------------------------------------------------------------------------
// N4 —— 非法名
// ---------------------------------------------------------------------------

/// **N4**：`create ../evil` / `create a/b` ⇒ 退出码非 0、stderr 含 `InvalidName`；
/// 且 `YUSHAN_WORKTREES_DIR` 下**无新增目录**（校验先于任何落点拼接）。
#[test]
fn n4_invalid_names_rejected_without_side_effects() {
    let sb = Sandbox::new("n4");
    init_repo(&sb.repo);

    for bad in ["../evil", "a/b"] {
        let out = run(&sb, &sb.repo, &["worktree", "create", bad]);
        let (_, stderr) = streams(&out);
        assert_ne!(out.status.code(), Some(0), "非法名 `{bad}` 应失败；stderr:\n{stderr}");
        assert!(
            stderr.contains("InvalidName"),
            "非法名 `{bad}` 的 stderr 应含 `InvalidName`：\n{stderr}"
        );
    }

    let count = std::fs::read_dir(&sb.trees).unwrap().count();
    assert_eq!(count, 0, "非法名不得在 worktrees 根下留下任何目录");
}

// ---------------------------------------------------------------------------
// N5 —— `list`
// ---------------------------------------------------------------------------

/// **N5**：空仓 `list` ⇒ 退出码 0 且 stdout 为空；建两棵树后 `list` ⇒
/// stdout 含两个名字与两条路径（canonical）。
#[test]
fn n5_list_shows_names_and_paths_and_empty_repo_is_ok() {
    let sb = Sandbox::new("n5");
    init_repo(&sb.repo);

    // 空仓：退出码 0、stdout 为空（仓库头走 stderr）。
    let empty = run(&sb, &sb.repo, &["worktree", "list"]);
    let (empty_out, empty_err) = streams(&empty);
    assert_eq!(empty.status.code(), Some(0), "空仓 list 应 exit(0)；stderr:\n{empty_err}");
    assert!(empty_out.is_empty(), "空仓 list 的 stdout 应为空：{empty_out:?}");

    let (feat_path, _) = create(&sb, "feat");
    let (bug_path, _) = create(&sb, "bug");

    let out = run(&sb, &sb.repo, &["worktree", "list"]);
    let (stdout, stderr) = streams(&out);
    assert_eq!(out.status.code(), Some(0), "list 应 exit(0)；stderr:\n{stderr}");

    for name in ["feat", "bug"] {
        assert!(
            stdout.contains(&format!("{name}\t")),
            "list 的 stdout 应含名字 `{name}`（制表符分隔的首列）：\n{stdout}"
        );
    }
    for path in [&feat_path, &bug_path] {
        let want = canonical(path);
        let want = want.to_str().unwrap();
        assert!(stdout.contains(want), "list 的 stdout 应含路径 {want:?}：\n{stdout}");
    }
}

// ---------------------------------------------------------------------------
// N6 —— `remove` 留分支、脏则拒删
// ---------------------------------------------------------------------------

/// **N6**：① 树内改一个文件（未提交）⇒ `remove` 退出码非 0 + 提示、树仍在；
/// ② `git commit` 后 `remove` ⇒ 退出码 0、树目录消失、**分支 `worktree-feat` 仍在**。
///
/// 变异靶子：把 `remove` 的「脏则拒删」改成无条件 `--force` ⇒ ① 退出码变 0 ⇒ 红。
#[test]
fn n6_remove_refuses_dirty_and_keeps_branch() {
    let sb = Sandbox::new("n6");
    init_repo(&sb.repo);

    let (tree, _) = create(&sb, "feat");

    // ① 未提交改动 ⇒ 拒删。
    std::fs::write(tree.join("README.md"), "modified\n").unwrap();
    let dirty = run(&sb, &sb.repo, &["worktree", "remove", "feat"]);
    let (_, dirty_err) = streams(&dirty);
    assert_ne!(
        dirty.status.code(),
        Some(0),
        "脏树 remove 应失败；stderr:\n{dirty_err}"
    );
    assert!(
        dirty_err.contains("删除 worktree 失败"),
        "stderr 应给出失败提示：\n{dirty_err}"
    );
    assert!(tree.is_dir(), "拒删后树应仍存在：{tree:?}");

    // ② 提交后再删 ⇒ 成功、目录消失、分支保留。
    let add = git(&tree, &["add", "-A"]);
    assert!(add.status.success(), "树内 git add 应成功");
    let commit = git(&tree, &["commit", "-q", "-m", "wip"]);
    assert!(
        commit.status.success(),
        "树内 git commit 应成功：{}",
        String::from_utf8_lossy(&commit.stderr)
    );

    let clean = run(&sb, &sb.repo, &["worktree", "remove", "feat"]);
    let (_, clean_err) = streams(&clean);
    assert_eq!(clean.status.code(), Some(0), "干净树 remove 应 exit(0)；stderr:\n{clean_err}");
    assert!(!tree.exists(), "remove 后树目录应消失：{tree:?}");

    let branch = git(&sb.repo, &["branch", "--list", "worktree-feat"]);
    assert!(
        String::from_utf8_lossy(&branch.stdout).contains("worktree-feat"),
        "remove 应**保留**分支 worktree-feat"
    );
}

// ---------------------------------------------------------------------------
// F1 —— 干净树直接 remove；真实改动仍拒删
// ---------------------------------------------------------------------------

/// **F1**：`create` 只做 `git worktree add`，树**天然干净**（无任何我们自建的制品）⇒
/// 直接 `remove` 即成功（退出码 0、目录消失、分支保留），无需任何强制选项；
/// 而树内有**真实未提交改动**时 `remove` 仍被拒（非 0、树仍在）。
///
/// 变异靶子：把 `remove` 改成自动加强制选项 ⇒ ② 的退出码变 0 ⇒ 红。
#[test]
fn clean_tree_removes_plainly_but_real_changes_refuse() {
    let sb = Sandbox::new("f1");
    init_repo(&sb.repo);
    // 主仓有非空的 `.yushan/skills/x`（空目录 git 看不见）—— 证明项目级配置**不妨碍**建树/删树，
    // 也**不会**被搬进树里（无软链）。
    let project = sb.repo.join(".yushan").join("skills").join("x");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("SKILL.md"), "x\n").unwrap();

    // ① 建树后直接删：树天然干净 ⇒ 一次 `git worktree remove` 即成功。
    let (tree, _) = create(&sb, "feat");
    assert!(
        std::fs::symlink_metadata(tree.join(".yushan")).is_err(),
        "树内不应有 `.yushan`（不建软链）：{tree:?}"
    );
    let st = git(&tree, &["status", "--porcelain"]);
    assert!(
        String::from_utf8_lossy(&st.stdout).trim().is_empty(),
        "新树应天然干净；status:\n{}",
        String::from_utf8_lossy(&st.stdout)
    );

    let clean = run(&sb, &sb.repo, &["worktree", "remove", "feat"]);
    let (_, clean_err) = streams(&clean);
    assert_eq!(
        clean.status.code(),
        Some(0),
        "干净树 remove 应 exit(0)；stderr:\n{clean_err}"
    );
    assert!(!tree.exists(), "remove 后树目录应消失：{tree:?}");
    let branch = git(&sb.repo, &["branch", "--list", "worktree-feat"]);
    assert!(
        String::from_utf8_lossy(&branch.stdout).contains("worktree-feat"),
        "remove 应**保留**分支 worktree-feat"
    );

    // ② 真实未提交改动 ⇒ 仍拒删。
    let (tree2, _) = create(&sb, "bug");
    std::fs::write(tree2.join("README.md"), "modified\n").unwrap();
    let dirty = run(&sb, &sb.repo, &["worktree", "remove", "bug"]);
    let (_, dirty_err) = streams(&dirty);
    assert_ne!(
        dirty.status.code(),
        Some(0),
        "有真实改动的树应拒删（非 0）；stderr:\n{dirty_err}"
    );
    assert!(tree2.is_dir(), "拒删后树应仍存在：{tree2:?}");
}

// ---------------------------------------------------------------------------
// N8 —— `attach` 降级（不 panic）
// ---------------------------------------------------------------------------

/// **N8**：树不存在 / 非 git 目录时 `worktree attach ghost -p hi`。
///
/// 断言：不 panic（子进程不崩、退出码非 101、stderr 无 `panicked`）、stderr 含警告，
/// 且**继续走到启动路径** —— 用不带凭证时的 `No model configured` 早退（exit 1）作证。
///
/// 变异靶子：把降级分支的 `eprintln!("警告：…")` 删掉或改成再次 `exit` ⇒ 对应断言红。
#[test]
fn n8_attach_degrades_without_panicking() {
    let sb = Sandbox::new("n8");

    // 情形一：在 git 仓内 attach 一棵不存在的树。
    init_repo(&sb.repo);
    let missing = run(&sb, &sb.repo, &["worktree", "attach", "ghost", "-p", "hi"]);
    let (_, m_err) = streams(&missing);
    assert!(
        m_err.contains("无法绑定工作树"),
        "树不存在应打降级警告；stderr:\n{m_err}"
    );
    assert!(!m_err.contains("panicked"), "降级不得 panic：\n{m_err}");
    assert_ne!(missing.status.code(), Some(101), "降级不得以 panic 收场：\n{m_err}");
    assert!(
        m_err.contains("No model configured"),
        "降级后应继续走到启动路径（无凭证 ⇒ 早退）；stderr:\n{m_err}"
    );
    assert_eq!(missing.status.code(), Some(1), "无凭证早退应为 exit(1)；stderr:\n{m_err}");

    // 情形二：在**非 git** 目录 attach。
    let nongit = run(&sb, &sb.plain, &["worktree", "attach", "ghost", "-p", "hi"]);
    let (_, n_err) = streams(&nongit);
    assert!(
        n_err.contains("无法绑定工作树"),
        "非 git 目录应打降级警告；stderr:\n{n_err}"
    );
    assert!(!n_err.contains("panicked"), "降级不得 panic：\n{n_err}");
    assert_ne!(nongit.status.code(), Some(101), "降级不得以 panic 收场：\n{n_err}");
    assert_eq!(nongit.status.code(), Some(1), "无凭证早退应为 exit(1)；stderr:\n{n_err}");
}

// ---------------------------------------------------------------------------
// N9 —— 退出行（`session:` + `worktree:`）
// ---------------------------------------------------------------------------

/// 回环地址上的极简 **成功** mock：每个请求都回 [`success_sse`]。
///
/// 与 `session_cli.rs` 的 `SseMockServer` 同形（不记录请求 —— N9 只关心退出行）。
/// 之所以能**离线**跑通成功回合：只有「回合成功」才会走 `persist_single_turn`，
/// 而 `worktree: <name>` 那一行只在那里打印。
struct SseServer {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl SseServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .expect("bind 回环 SSE mock（沙箱可能禁止 bind）");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let sse = success_sse();
        let handle = std::thread::spawn(move || {
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = serve_sse_once(stream, &sse);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            port,
            stop,
            handle: Some(handle),
        }
    }

    /// api_base（code 会在其后追加 `/chat/completions`）。
    fn api_base(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }
}

impl Drop for SseServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 一段最简成功 SSE（逐字照搬 `session_cli.rs::success_sse` 的帧形状）。
fn success_sse() -> String {
    let mut sse = String::new();
    sse.push_str(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
    );
    sse.push_str("data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n");
    sse.push_str("data: [DONE]\n\n");
    sse
}

fn headers_content_length(header_bytes: &[u8]) -> usize {
    let text = String::from_utf8_lossy(header_bytes);
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.eq_ignore_ascii_case("content-length") {
            return value.trim().parse::<usize>().unwrap_or(0);
        }
    }
    0
}

/// 读一个请求（行 + 头 + body），回 200 + SSE 后关闭。
fn serve_sse_once(mut stream: std::net::TcpStream, sse: &str) -> std::io::Result<()> {
    // macOS/BSD 上 `accept()` 的连接会继承监听 socket 的 O_NONBLOCK，显式复位。
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    let mut raw = Vec::new();
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        raw.extend_from_slice(&tmp[..n]);
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let content_length = headers_content_length(&raw[..header_end]);
    while raw.len() < header_end + content_length {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&tmp[..n]);
    }

    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        sse.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(sse.as_bytes())?;
    stream.flush()
}

/// **N9**：worktree 会话跑 `-p` ⇒ stderr **同时**含 `session: <id>` 与 `worktree: feat`；
/// 非 worktree 会话 ⇒ 只有 `session:`、**没有** `worktree:`。
///
/// 用回环 SSE mock 跑通成功回合（`persist_single_turn` 才会执行），故**无需真实凭证**。
/// 变异靶子：删掉 `persist_single_turn` 里的 `worktree` 分支 ⇒ 第一段断言红。
#[test]
fn n9_exit_lines_report_worktree_only_when_attached() {
    let server = SseServer::start();
    let sb = Sandbox::new("n9");
    init_repo(&sb.repo);
    let _ = create(&sb, "feat");

    let api_base = server.api_base();

    // worktree 会话：attach 到树后跑 `-p`。
    let mut cmd = command(&sb, &sb.repo);
    cmd.args(["worktree", "attach", "feat", "-p", "hi"])
        .env("YUSHAN_API_BASE", &api_base)
        .env("YUSHAN_API_KEY", "sk-test")
        .env("YUSHAN_MODEL", "mock-model");
    let out = run_cmd(cmd, &["worktree", "attach", "feat", "-p", "hi"]);
    let (stdout, stderr) = streams(&out);
    assert_eq!(out.status.code(), Some(0), "成功回合应 exit(0)；stderr:\n{stderr}");
    assert!(stdout.contains("hi"), "stdout 应为模型文本；stdout:\n{stdout:?}");
    assert!(stderr.contains("session: "), "stderr 应含 `session: <id>`：\n{stderr}");
    assert!(
        stderr.contains("worktree: feat"),
        "worktree 会话的 stderr 应含 `worktree: feat`：\n{stderr}"
    );

    // 非 worktree 会话：`-p` 直跑 ⇒ 只有 `session:`。
    let mut cmd = command(&sb, &sb.repo);
    cmd.args(["-p", "hi"])
        .env("YUSHAN_API_BASE", &api_base)
        .env("YUSHAN_API_KEY", "sk-test")
        .env("YUSHAN_MODEL", "mock-model");
    let out = run_cmd(cmd, &["-p", "hi"]);
    let (_, plain_err) = streams(&out);
    assert_eq!(out.status.code(), Some(0), "成功回合应 exit(0)；stderr:\n{plain_err}");
    assert!(plain_err.contains("session: "), "stderr 应含 `session: <id>`：\n{plain_err}");
    assert!(
        !plain_err.contains("worktree: "),
        "非 worktree 会话**不得**打 `worktree:` 行：\n{plain_err}"
    );
}

// ---------------------------------------------------------------------------
// A1 —— `attach` 的项目配置缺失警告
// ---------------------------------------------------------------------------

/// **A1**：主仓有 `.yushan/` ⇒ `attach` 成功注入工作区后向 stderr 打缺失警告；
/// 主仓**无** `.yushan/` ⇒ **不打任何警告**（不制造噪声）。
///
/// 用无凭证时的 `No model configured` 早退（exit 1）证明 attach 成功并继续走到启动路径
/// —— 警告在 attach 注入后、模型装配前打印。
///
/// 变异靶子：删掉 `main.rs` 里 attach 成功分支的警告 `eprintln!` ⇒ 情形一红；
/// 把条件写成恒真 ⇒ 情形二红。
#[test]
fn attach_warns_when_main_repo_has_project_config_only() {
    // 情形一：主仓有 `.yushan/`（目录存在即可）⇒ 打警告。
    let sb = Sandbox::new("attach_cfg_yes");
    init_repo(&sb.repo);
    std::fs::create_dir_all(sb.repo.join(".yushan")).unwrap();
    let _ = create(&sb, "feat");

    let out = run(&sb, &sb.repo, &["worktree", "attach", "feat", "-p", "hi"]);
    let (_, stderr) = streams(&out);
    assert_eq!(out.status.code(), Some(1), "无凭证应早退 exit(1)；stderr:\n{stderr}");
    assert!(
        stderr.contains("本会话中不可见"),
        "主仓有 `.yushan/` 时 attach 应打缺失警告；stderr:\n{stderr}"
    );
    assert!(!stderr.contains("panicked"), "警告路径不得 panic：\n{stderr}");

    // 情形二：主仓无 `.yushan/` ⇒ 不打。
    let sb2 = Sandbox::new("attach_cfg_no");
    init_repo(&sb2.repo);
    let _ = create(&sb2, "feat");

    let out = run(&sb2, &sb2.repo, &["worktree", "attach", "feat", "-p", "hi"]);
    let (_, stderr2) = streams(&out);
    assert_eq!(out.status.code(), Some(1), "无凭证应早退 exit(1)；stderr:\n{stderr2}");
    assert!(
        !stderr2.contains("本会话中不可见"),
        "主仓无 `.yushan/` 时**不得**打缺失警告；stderr:\n{stderr2}"
    );
}

// ---------------------------------------------------------------------------
// U3 —— `--resume` 恢复到会话记录的工作树
// ---------------------------------------------------------------------------

/// 给 mock 回合注入回环凭证（成功回合才会走 `persist_single_turn`）。
fn with_mock(cmd: &mut Command, api_base: &str) {
    cmd.env("YUSHAN_API_BASE", api_base)
        .env("YUSHAN_API_KEY", "sk-test")
        .env("YUSHAN_MODEL", "mock-model");
}

/// 建树 → `worktree attach <name> -p hi` 跑一个成功回合 → 返回（会话 id，树路径）。
///
/// 成功回合是前提：meta 与 `session: <id>` 都只在 `persist_single_turn` 里写。
fn attach_and_run(sb: &Sandbox, name: &str, api_base: &str) -> (String, PathBuf) {
    let (tree, _) = create(sb, name);
    let mut cmd = command(sb, &sb.repo);
    cmd.args(["worktree", "attach", name, "-p", "hi"]);
    with_mock(&mut cmd, api_base);
    let out = run_cmd(cmd, &["worktree", "attach", name, "-p", "hi"]);
    let (_, stderr) = streams(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "attach -p 成功回合应 exit(0)；stderr:\n{stderr}"
    );
    let id = line_after(&stderr, "session: ")
        .expect("成功回合应打 `session: <id>`")
        .trim()
        .to_string();
    (id, tree)
}

/// **U3-①②**：attach 会话成功回合后 meta **记录**了树路径；随后不带 attach 的
/// `--resume <id>` **回到该树**并告知。
///
/// 变异靶子：`finalize_turn_meta` 不写 `workspace` ⇒ ① 红；启动期不读 meta 工作区 ⇒ ② 红。
#[test]
fn u3_resume_restores_recorded_worktree() {
    let server = SseServer::start();
    let sb = Sandbox::new("u3_restore");
    init_repo(&sb.repo);
    let api_base = server.api_base();

    let (id, tree) = attach_and_run(&sb, "feat", &api_base);

    // ① meta 记录了该树路径。
    let meta_path = sb.sessions.join(format!("{id}.jsonl"));
    let content = std::fs::read_to_string(&meta_path).expect("会话文件应存在");
    let first = content.lines().next().expect("首行应存在");
    assert!(
        first.contains("\"workspace\""),
        "meta 应含 workspace 键：\n{first}"
    );
    assert!(
        first.contains(tree.to_str().unwrap()),
        "meta 应记录树路径 {}：\n{first}",
        tree.display()
    );

    // ② 不带 attach 的 `--resume <id>` ⇒ 恢复回该树（cwd 从共享仓出发，证明是恢复而非沿用 cwd）。
    let mut cmd = command(&sb, &sb.repo);
    cmd.args(["-p", "--resume", &id, "hi"]);
    with_mock(&mut cmd, &api_base);
    let out = run_cmd(cmd, &["-p", "--resume", &id, "hi"]);
    let (_, stderr) = streams(&out);
    assert_eq!(out.status.code(), Some(0), "恢复回合应 exit(0)；stderr:\n{stderr}");
    assert!(
        stderr.contains("已恢复到工作树：feat"),
        "应告知恢复到工作树：\n{stderr}"
    );
    assert!(
        stderr.contains("worktree: feat"),
        "恢复后退出行应报告当前工作树：\n{stderr}"
    );
}

/// **U3-③**：记录的工作树被删除后 `--resume <id>` ⇒ 明确告知「已不存在」，不崩，
/// 且按共享工作区继续把回合跑完（退出码 0）。
#[test]
fn u3_resume_reports_missing_recorded_worktree() {
    let server = SseServer::start();
    let sb = Sandbox::new("u3_missing");
    init_repo(&sb.repo);
    let api_base = server.api_base();

    let (id, tree) = attach_and_run(&sb, "feat", &api_base);

    // 删掉整棵工作树后再 resume。
    std::fs::remove_dir_all(&tree).expect("删树应成功");
    assert!(!tree.is_dir(), "树应已删除：{tree:?}");

    let mut cmd = command(&sb, &sb.repo);
    cmd.args(["-p", "--resume", &id, "hi"]);
    with_mock(&mut cmd, &api_base);
    let out = run_cmd(cmd, &["-p", "--resume", &id, "hi"]);
    let (_, stderr) = streams(&out);
    assert!(!stderr.contains("panicked"), "缺失树不得 panic：\n{stderr}");
    assert_ne!(out.status.code(), Some(101), "缺失树不得以 panic 收场：\n{stderr}");
    assert!(
        stderr.contains("记录的工作树已不存在"),
        "应告知记录的工作树已不存在：\n{stderr}"
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "回合仍应在共享工作区跑完（exit 0）；stderr:\n{stderr}"
    );
}

/// **U3-④**：显式 `attach` 与 meta 记录**冲突**时显式者优先，并说明二者不同。
#[test]
fn u3_explicit_attach_wins_over_recorded_worktree() {
    let server = SseServer::start();
    let sb = Sandbox::new("u3_conflict");
    init_repo(&sb.repo);
    let api_base = server.api_base();

    // 先在 `feat` 树里跑一个会话（记录工作树 = feat）。
    let (id, _feat) = attach_and_run(&sb, "feat", &api_base);

    // 再显式 attach 到另一棵树 `bug` 并 resume 同一会话。
    let _ = create(&sb, "bug");
    let mut cmd = command(&sb, &sb.repo);
    cmd.args(["worktree", "attach", "bug", "-p", "--resume", &id, "hi"]);
    with_mock(&mut cmd, &api_base);
    let out = run_cmd(cmd, &["worktree", "attach", "bug", "-p", "--resume", &id, "hi"]);
    let (_, stderr) = streams(&out);
    assert_eq!(out.status.code(), Some(0), "成功回合应 exit(0)；stderr:\n{stderr}");
    assert!(
        stderr.contains("已按显式 attach 使用"),
        "显式 attach 与记录不同时应说明：\n{stderr}"
    );
    assert!(
        stderr.contains("worktree: bug"),
        "退出行应报告显式 attach 的树（显式者优先）：\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// U5-4a —— 陈旧登记的 prune（rm -rf 后同名 create 执行 prune）
// ---------------------------------------------------------------------------

/// **U5-4a**：① 建树 → ② 直接 `rm -rf` 掉树目录（不走 `remove`，使 git 仍持有陈旧登记）
/// → ③ 再 `create` 同名。
///
/// 本机 git 下这条**重试不会成功**：失败的首个 `worktree add -b` 会留下同名分支，而 `prune`
/// 不删分支 ⇒ 重试必撞「分支已存在」。故本用例只钉住**可观测的 prune 副作用** —— `create`
/// 以失败告终后，陈旧登记已被 `prune` 从 git 清除（`worktree list` 不再含该落点），且错误提示
/// 落在「分支已存在 ⇒ 换名」一侧。谓词本身由 `worktree.rs` 的单测
/// `is_stale_registration_tracks_registered_but_missing_dir` 覆盖。
///
/// 变异靶子：删掉 `create` 里 `if is_stale_registration { … prune … }` 块 ⇒ ③ 后该落点仍是
/// 陈旧登记 ⇒ 断言红。
#[test]
fn create_prunes_stale_registration_after_dir_removed() {
    let sb = Sandbox::new("u5_prune");
    init_repo(&sb.repo);

    let (tree, _) = create(&sb, "feat");
    assert!(tree.is_dir(), "树应建成：{tree:?}");

    // ② 直接删目录（不走 remove ⇒ git 仍登记该落点）。
    std::fs::remove_dir_all(&tree).unwrap();
    assert!(!tree.exists(), "树目录应已删除：{tree:?}");

    // ③ 再 create 同名 ⇒ 走 is_stale_registration 分支（prune 后重试，但分支仍在 ⇒ 仍失败）。
    let out = run(&sb, &sb.repo, &["worktree", "create", "feat"]);
    let (_, stderr) = streams(&out);
    assert_ne!(
        out.status.code(),
        Some(0),
        "同名分支仍在 ⇒ create 应失败；stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("创建 worktree 失败"),
        "应给出失败提示：\n{stderr}"
    );
    assert!(
        stderr.contains("请换一个名字"),
        "失败原因应落在「分支已存在 ⇒ 换名」一侧：\n{stderr}"
    );

    // prune 生效：陈旧登记已从 git 清除（落点不再出现在 worktree list）。
    let list = git(&sb.repo, &["worktree", "list", "--porcelain"]);
    let listed = String::from_utf8_lossy(&list.stdout).into_owned();
    let want = canonical(&tree);
    let still_registered = listed
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .any(|p| canonical(Path::new(p.trim())) == want);
    assert!(
        !still_registered,
        "陈旧登记应已被 create 内部的 `git worktree prune` 清除：\n{listed}"
    );
}

// ---------------------------------------------------------------------------
// U5-4b —— 在 linked worktree 内取主仓根（--git-common-dir，非 --show-toplevel）
// ---------------------------------------------------------------------------

/// **U5-4b**：建树 → 在**树内**跑 `worktree list`（`main_repo` 用
/// `rev-parse --git-common-dir`，**不是** `--show-toplevel`）。
///
/// 断言：仓库头是**主仓**（名 `YuShan`、根 = 主仓 canonical 路径），且能列到该树；
/// 树内 `attach` 同一棵已存在的树也**不降级**。
///
/// 变异靶子：把 `main_repo` 的 `--git-common-dir` 换成 `--show-toplevel` ⇒ 树内运行时返回
/// **树根**（名 `feat`）⇒ 仓库头与落点子树都错 ⇒ 断言红。
#[test]
fn main_repo_is_resolved_from_inside_linked_worktree() {
    let sb = Sandbox::new("u5_mainrepo");
    init_repo(&sb.repo);
    let (tree, _) = create(&sb, "feat");

    // 在树内跑 list。
    let out = run(&sb, &tree, &["worktree", "list"]);
    let (stdout, stderr) = streams(&out);
    assert_eq!(out.status.code(), Some(0), "树内 list 应 exit(0)；stderr:\n{stderr}");

    let main = canonical(&sb.repo);
    assert!(
        stderr.contains("仓库：YuShan（"),
        "仓库头应为主仓（名 YuShan），不是树根派生名：\n{stderr}"
    );
    assert!(
        stderr.contains(main.to_str().unwrap()),
        "仓库头应给出主仓根 {}：\n{stderr}",
        main.display()
    );
    assert!(
        stdout.contains("feat\t"),
        "树内 list 应列到该树（证明落点按主仓分量算）：\n{stdout}"
    );
    assert!(
        !stderr.contains("仓库：feat"),
        "仓库名不得是树根派生的 `feat`：\n{stderr}"
    );

    // 树内 attach 同一棵已存在的树：目标解析应成功（不降级）。
    let out = run(&sb, &tree, &["worktree", "attach", "feat", "-p", "hi"]);
    let (_, a_err) = streams(&out);
    assert!(
        !a_err.contains("无法绑定工作树"),
        "树内 attach 已存在的树不应降级：\n{a_err}"
    );
}

// ---------------------------------------------------------------------------
// U5-4c —— 无参 attach 的 0 棵 / 2 棵候选提示
// ---------------------------------------------------------------------------

/// **U5-4c**：无参 `attach` 在 **0 棵**工作树时报「先 create」；在 **2 棵**时逐行列出两个
/// 候选（名字 + 路径）。两者都**降级**为共享工作区启动（不 panic、无凭证下 exit 1）。
///
/// 变异靶子：删掉候选清单 / 把文案退回单行 ⇒ 对应断言红。
#[test]
fn no_arg_attach_reports_zero_and_two_candidates() {
    let sb = Sandbox::new("u5_noarg");
    init_repo(&sb.repo);

    // 0 棵：提示先 create。
    let zero = run(&sb, &sb.repo, &["worktree", "attach", "-p", "hi"]);
    let (_, zero_err) = streams(&zero);
    assert!(
        zero_err.contains("无法绑定工作树"),
        "无参 attach 应降级告警：\n{zero_err}"
    );
    assert!(
        zero_err.contains("还没有工作树") && zero_err.contains("worktree create"),
        "0 棵时应提示先 `worktree create`：\n{zero_err}"
    );
    assert_eq!(zero.status.code(), Some(1), "无凭证应早退 exit(1)：\n{zero_err}");

    // 2 棵：列出两个候选（名字 + 路径）。
    let (feat, _) = create(&sb, "feat");
    let (bug, _) = create(&sb, "bug");
    let two = run(&sb, &sb.repo, &["worktree", "attach", "-p", "hi"]);
    let (_, two_err) = streams(&two);
    assert!(
        two_err.contains("无法绑定工作树"),
        "无参 attach（2 棵）应降级告警：\n{two_err}"
    );
    assert!(
        two_err.contains("有 2 个工作树"),
        "应说明候选数量：\n{two_err}"
    );
    for (name, path) in [("feat", &feat), ("bug", &bug)] {
        assert!(
            two_err.contains(name),
            "候选应含名字 `{name}`：\n{two_err}"
        );
        let p = canonical(path);
        assert!(
            two_err.contains(p.to_str().unwrap()),
            "候选应含路径 {}：\n{two_err}",
            p.display()
        );
    }
    assert_eq!(two.status.code(), Some(1), "无凭证应早退 exit(1)：\n{two_err}");
}
