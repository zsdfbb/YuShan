//! **T14 —— skill 启动期诊断的 CLI 级去向**（子进程 + 无模型凭证，零成本、可离线跑）。
//!
//! # 被测的生产路径
//!
//! `main()` 在**建 agent 之前**做三件事（`apps/coding-agent/src/main.rs`）：
//!
//! 1. `Config::default_skill_roots(cwd)` ⇒ `[<cwd>/.yushan/skills, <home>/.yushan/skills]`
//!    （**项目级在前** —— 位置即优先级，见 `config.rs` 的同名函数）；
//! 2. `config.load_skills(roots)` ⇒ `skills::scan` 的扫描 / 遮蔽 / 非法值诊断（**返回值**，不由该模块打印）；
//! 3. `startup_hints(...)` 把这些诊断与 `toolset` 降级提示、自定义 prompt 说明、R5 窗口警告
//!    **合并成一条**，去处按模式三分（`-p` / `--json` → **stderr**；TUI → transcript）。
//!
//! 本文件钉住的正是第 3 步里「skill 侧那几行」的去向 —— `skills.rs` 的单测覆盖扫描与诊断
//! **内容**，`main.rs` 的单测覆盖 `startup_hints` 的**合并**，但「`-p` / `--json` 下真的
//! 落在 stderr、且**不落** stdout」这条生产路径**只有这里走得到**。
//!
//! # 为什么无凭证也够
//!
//! 三条用例全部**不配任何模型凭证**：`ensure_configured` 会在 `eprintln!` 出
//! `No model configured` 后 `exit(1)`。这恰好是**最干净的观测量** —— 诊断（启动期）与
//! 早退（`exit(1)`）之间没有 turn、没有网络、没有事件流，故 stdout 上**任何**字节都只能
//! 来自「诊断泄漏到了错误的流」。真实 turn 的用法在 `tests/e2e_skills.rs`（T15）。
//!
//! # 子进程 env 隔离（照 `degraded_search.rs` 的规约）
//!
//! 全部经 `Command` 的 **per-child** `.env()` / `.env_remove()` / `.current_dir()` 注入，
//! **绝不改父进程 env**（改写进程级 env 会与同 binary 内并行测试的 env 读取产生数据竞争）。
//! 被清掉的键分两组：**模型侧**（不清则进程不会走到「无模型配置」早退，三条用例全失效）与
//! **web 侧 + PATH**（决定 `toolset` 降级提示出不出现 —— 不清就随开发机 shell 漂移）。
//!
//! `HOME` 指向**空临时目录**：它是 `default_skill_roots` 的第二条根，也是 `auth.json` /
//! `state.json` / `config.json` 的落点。空 ⇒ 用户级 skill 目录不存在（用例 1 例外，那里
//! **故意**放一份同名 skill 来触发遮蔽）。
//!
//! # 挂起即失败
//!
//! 无模型配置时进程应**毫秒级退出**。命令一律套 [`CHILD_TIMEOUT`]：挂起必须是 panic
//! 并带上已捕获的输出，而不是让测试永不结束。

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// 子进程退出上限。正常路径毫秒级返回；超时 = 挂起 = panic。
const CHILD_TIMEOUT: Duration = Duration::from_secs(30);

/// 进程内唯一临时目录：**原子序号 + `process::id()`**（并行安全，项目硬约定）。
///
/// `SystemTime::as_nanos()` 在 macOS 只到微秒级，并行测试会撞名互删 —— 故用计数器。
fn unique_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "yushan_skills_cli_{tag}_{}_{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 在 `<root>/<command>/SKILL.md` 落一份 skill（`root` 即某个 `.yushan/skills`）。
fn write_skill(root: &Path, command: &str, content: &str) {
    let dir = root.join(command);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), content).unwrap();
}

/// 建三条 root：项目级 `<ws>/.yushan/skills`、用户级 `<home>/.yushan/skills`，以及一个
/// **空的 PATH 目录**（把 `toolset` 的降级提示钉成常量，别随开发机装没装 ripgrep 漂移）。
struct Roots {
    workspace: PathBuf,
    home: PathBuf,
    path_dir: PathBuf,
}

impl Roots {
    fn new(tag: &str) -> Self {
        Self {
            workspace: unique_dir(&format!("{tag}_ws")),
            home: unique_dir(&format!("{tag}_home")),
            path_dir: unique_dir(&format!("{tag}_path")),
        }
    }

    fn project_skills(&self) -> PathBuf {
        self.workspace.join(".yushan").join("skills")
    }

    fn user_skills(&self) -> PathBuf {
        self.home.join(".yushan").join("skills")
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.workspace);
        let _ = std::fs::remove_dir_all(&self.home);
        let _ = std::fs::remove_dir_all(&self.path_dir);
    }
}

/// 所有用例共用的隔离（**全部 per-child**，不碰父进程 env）：
///
/// ① `HOME` → 空临时目录（无 `auth.json` / `state.json` / `config.json`，用户级 skill 根不存在）；
/// ② `current_dir` → 临时工作区（`Config::from_env` 的 `cwd` 取自 `env::current_dir()`，
///    它决定**项目级** skill 根）；
/// ③ **模型侧** `YUSHAN_*` 全清 —— 不清则进程带着开发机的凭证跑到真 turn，早退断言失效
///    （进而整个文件失去意义）；
/// ④ **web 侧**五个 `YUSHAN_SEARCH_*`、通用名 `TAVILY_API_KEY` / `DEEPSEEK_API_KEY`、
///    `YUSHAN_FETCHED_DIR` 全清 —— 决定
///    `WebConfig::degraded_hint` 的那句话出不出现（键名逐字取自 `adapters/tools-web` 的 `ENV_*`）；
/// ⑤ `PATH` → 空目录 —— 决定 `toolset` 侧降级提示恒定出现。
///
/// ④⑤ 与本文件的三条 skill 断言**无关**，清掉只为让 stderr 的内容**完全由被测变量决定**。
fn isolate(cmd: &mut Command, roots: &Roots) {
    cmd.current_dir(&roots.workspace)
        .env_remove("YUSHAN_LOG_DIR")
        .env("YUSHAN_DEBUG", "0")
        .env("HOME", &roots.home)
        .env("PATH", &roots.path_dir)
        .env_remove("USERPROFILE")
        // ③ 模型侧
        .env_remove("YUSHAN_API_BASE")
        .env_remove("YUSHAN_API_KEY")
        .env_remove("YUSHAN_MODEL")
        .env_remove("YUSHAN_SESSIONS_DIR")
        .env_remove("YUSHAN_CHANNEL_CAPACITY")
        // ④ web 侧
        .env_remove("YUSHAN_SEARCH_SEARXNG_URL")
        .env_remove("YUSHAN_SEARCH_TAVILY_KEY")
        // Tavily 的生态通用名：产品会作为回退读取，不清则随开发机 shell 漂移。
        .env_remove("TAVILY_API_KEY")
        .env_remove("YUSHAN_SEARCH_DEEPSEEK_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("YUSHAN_SEARCH_BOCHA_KEY")
        .env_remove("YUSHAN_SEARCH_FIRECRAWL_KEY")
        .env_remove("YUSHAN_FETCHED_DIR");
}

/// 跑一次被测二进制（**套 timeout**）。挂起 → panic（带已捕获输出），而非测试挂死。
fn run_guarded(argv: &[&str], roots: &Roots) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ys-coding-agent"));
    cmd.args(argv);
    isolate(&mut cmd, roots);

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

/// 「skill 诊断字样」的判别集 —— 只收**只有 skill 诊断才会出现**的锚点。
///
/// **刻意不用裸 `skill`**：R5 的窗口警告（`fixed_cost_note`）里有一句
/// 「或给部分 skill 标 `disable-model-invocation`」，那是**合法的**启动期提示，
/// 用它当判别词会让「无 skills 目录」用例随机变红。下列四个锚点各自逐字来自
/// `skills.rs` / `config.rs`，R5 与降级提示都不含它们：
///
/// | 锚点 | 出处 |
/// |---|---|
/// | `skill /` | `scan_root` 与 `shadow_diagnostics` 的诊断前缀（`"skill /{command}: …"`） |
/// | `SKILL.md` | 遮蔽诊断尾部的「保留 &lt;path&gt;」 |
/// | `遮蔽` | `shadow_diagnostics` |
/// | `frontmatter` | `scan_root` 的非法值诊断 |
const SKILL_DIAG_MARKERS: [&str; 4] = ["skill /", "SKILL.md", "遮蔽", "frontmatter"];

/// 返回 stderr 里命中的 skill 诊断锚点（空 = 一条都没有）。
fn skill_markers_in(stderr: &str) -> Vec<&'static str> {
    SKILL_DIAG_MARKERS
        .iter()
        .copied()
        .filter(|m| stderr.contains(m))
        .collect()
}

/// **用例 1 —— `-p` 下 skill 诊断必须出现在 stderr。**
///
/// 一次跑出**两类**诊断（两条诊断路径各钉一次）：
///
/// ① **同名遮蔽**（`shadow_diagnostics`）：项目级与用户级各有一份 `proj-x` ⇒
///    `project` 胜、`user` 被整条替换 ⇒ 一行
///    `skill /proj-x: project 级遮蔽 user 级（保留 <绝对路径>/SKILL.md）`。
///    这一条**同时**证明了两件事：`default_skill_roots` 的两条根**都**被扫了（否则没有同名），
///    以及顺序语义（项目在前 ⇒ 项目胜）。
/// ② **非法值丢弃**（`scan_root` 的 `fm.dropped`）：`description: [bad` 是 YAML 流集合、
///    **不是字符串** ⇒ 字段被丢（该 skill 因此整条不加载）⇒ 一行
///    `skill /broken-y: frontmatter 字段 \`description\` 的值以 \`[\` 开头（非字符串），已丢弃`。
///
/// 断言用的是**命令名级别**的内容（`skill /proj-x`、`skill /broken-y`），而非
/// `is_some()` —— 硬编码文案、丢诊断、把两个 skill 写混都会红。
///
/// 无凭证 ⇒ 进程以 `exit(1)` 收场：这一并证明了诊断来自**启动期**（早退之前）而非别处。
///
/// 变异靶子：
/// - `main.rs` 把 `eprintln!("{hint}")` 改成 `println!` ⇒ 本用例的 `stdout.is_empty()`
///   与用例 2 的逐行 JSON 断言同时变红。
/// - `startup_hints` 不再 `extend(skill_diags)`（skill 侧被吞掉）⇒ 前两条断言变红。
/// - `skills.rs` 去掉非法值诊断（`fm.dropped` 不产线）⇒ `broken-y` 那条断言变红。
/// - 遮蔽诊断丢掉胜者路径（`（保留 …）`）⇒ `SKILL.md` 断言变红。
#[test]
fn cli_skill_diagnostics_go_to_stderr_in_print_mode() {
    let roots = Roots::new("print");

    // ① 项目级 proj-x（合法）
    write_skill(
        &roots.project_skills(),
        "proj-x",
        "---\nname: proj-x\ndescription: 项目级描述\n---\n项目正文\n",
    );
    // ① 用户级同名 proj-x（合法）⇒ 触发遮蔽诊断
    write_skill(
        &roots.user_skills(),
        "proj-x",
        "---\nname: proj-x\ndescription: 用户级描述\n---\n用户正文\n",
    );
    // ② 项目级 broken-y（description 是流集合 ⇒ 字段被丢 ⇒ 诊断）
    write_skill(
        &roots.project_skills(),
        "broken-y",
        "---\nname: broken-y\ndescription: [bad\n---\n正文\n",
    );

    let out = run_guarded(&["-p", "hi"], &roots);
    let (stdout, stderr) = streams(&out);

    // 诊断**必须**在 stderr 上（两条路径各钉一条，且点名到 command）。
    assert!(
        stderr.contains("skill /proj-x"),
        "stderr 应含 proj-x 的遮蔽诊断（命令名 = 目录名）；stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("级遮蔽"),
        "遮蔽诊断应说明谁遮蔽谁；stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("SKILL.md"),
        "遮蔽诊断应给出胜者路径（保留 …/SKILL.md）；stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("skill /broken-y"),
        "stderr 应含 broken-y 的非法值诊断；stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("已丢弃"),
        "非法值诊断应说明字段已被丢弃；stderr:\n{stderr}"
    );

    // 诊断**不得**落在 stdout：`-p` 的 stdout 是给用户看的模型文本流。
    assert!(
        stdout.is_empty(),
        "启动期诊断不得写 stdout（--json 上是逐行 Envelope，-p 上是模型文本）；\
         stdout:\n{stdout:?}"
    );

    // 前端（无模型配置）应已正常走到早退 ⇒ 佐证诊断来自**启动期**。
    assert!(!out.status.success(), "无模型配置应非零退出");
    assert!(
        stderr.contains("No model configured"),
        "进程应走到「无模型配置」早退；stderr:\n{stderr}"
    );

    roots.cleanup();
}

/// **用例 2 —— `--json` 的 stdout 纯净性：诊断绝不能混进 JSON 流。**
///
/// 条件与用例 1 完全相同（同样的两个诊断、同样无凭证），**只换模式**。
/// 这样「唯一被改的变量」是输出模式，故 `--json` 下任何非 JSON 的 stdout 行都只能是
/// **泄漏**，不可能是别的原因。
///
/// 断言两层：
/// 1. **逐行 JSON 校验** —— 每个非空行都必须 `serde_json::from_str::<Value>` 通过。
///    这是下游消费者（任何解析 `Envelope` 的工具）的契约：一行坏 JSON 即整条管道崩。
/// 2. **stdout 为空** —— 无模型 ⇒ 一个 turn 都跑不起来 ⇒ 本不该有任何 stdout。
///    它与 1 不重复：1 允许「合法的 JSON 行」，2 禁止「任何行」。两条一起，
///    「诊断改道 stdout」与「莫名多出合法 JSON」两种变异都能被抓。
///
/// 变异靶子：`main.rs` 的 `eprintln!` → `println!`（诊断进 stdout）⇒ 1 与 2 同时变红。
#[test]
fn cli_json_stdout_stays_pure_json_when_skill_diagnostics_fire() {
    let roots = Roots::new("json");
    write_skill(
        &roots.project_skills(),
        "proj-x",
        "---\nname: proj-x\ndescription: 项目级描述\n---\n项目正文\n",
    );
    write_skill(
        &roots.user_skills(),
        "proj-x",
        "---\nname: proj-x\ndescription: 用户级描述\n---\n用户正文\n",
    );
    write_skill(
        &roots.project_skills(),
        "broken-y",
        "---\nname: broken-y\ndescription: [bad\n---\n正文\n",
    );

    let out = run_guarded(&["--json", "hi"], &roots);
    let (stdout, stderr) = streams(&out);

    // 先证明**本轮确实产出了 skill 诊断**（在 stderr 上）—— 否则下面的纯 JSON 断言
    // 会退化成「什么都没发生」的空转，抓不住任何变异。
    assert!(
        stderr.contains("skill /proj-x") && stderr.contains("skill /broken-y"),
        "本用例的前置：两个 skill 诊断必须已在 stderr 上出现（否则断言空转）；\
         stderr:\n{stderr}"
    );

    // 1. stdout 的每一非空行都必须是合法 JSON。
    for (index, line) in stdout.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        if let Err(err) = serde_json::from_str::<serde_json::Value>(line) {
            panic!(
                "--json 的 stdout 第 {index} 行不是合法 JSON（诊断/日志泄漏进了事件流）：\
                 {err}\n该行原文：{line:?}\n完整 stdout:\n{stdout}"
            );
        }
    }

    // 2. 且 stdout 应当**一行都没有**（无模型 ⇒ 无 turn ⇒ 无事件）。
    assert!(
        stdout.is_empty(),
        "--json 的 stdout 必须是纯 Envelope 流，任何非 JSON 字节都是泄漏；stdout:\n{stdout:?}"
    );

    assert!(!out.status.success(), "无模型配置应非零退出");
    assert!(
        stderr.contains("No model configured"),
        "进程应走到「无模型配置」早退；stderr:\n{stderr}"
    );

    roots.cleanup();
}

/// **用例 3 —— 没有 skills 目录 ⇒ 零 skill 诊断、行为不变。**
///
/// 同一工作区跑两次，**唯一被改的变量**是 `.yushan/skills` 存不存在：
/// - 跑 A：工作区里**根本不建** `.yushan/skills`（用户级根也不存在）；
/// - 跑 B：建一个**空的** `.yushan/skills`（`scan` 对空目录 / 不存在的目录都是
///   **零条目零诊断**，见 `skills.rs::scan_root` 的早返回）。
///
/// 断言三件事：
/// 1. 两次的 stderr 里**没有任何 skill 诊断锚点**（见 [`SKILL_DIAG_MARKERS`]）；
/// 2. 两次的 **stdout 皆空**；
/// 3. 两次的 **stderr 逐字节相同** —— 「行为不变」的直接表述。空 skills 目录若意外
///    产生了什么（诊断、路径回显、额外提示），3 会立刻红。
///
/// `PATH` / `HOME` 两次相同，故「工具降级提示」「web 侧提示」在两次里也逐字节相同 ——
/// 差异只可能来自 skills 目录。
///
/// 变异靶子：`scan_root` 对空目录也产一条诊断 ⇒ 1 红；`load_skills` 在零 skill 时也
/// 往 `startup_hints` 里塞东西 ⇒ 3 红。
#[test]
fn cli_no_skill_diagnostics_when_skills_dir_absent_or_empty() {
    let roots = Roots::new("noskills");

    // 跑 A：工作区里没有 `.yushan/skills`。
    assert!(
        !roots.project_skills().exists(),
        "前置：本用例**不得**替项目级建出 skills 目录"
    );
    let a = run_guarded(&["-p", "hi"], &roots);
    let (a_stdout, a_stderr) = streams(&a);

    // 跑 B：建一个**空**的 `.yushan/skills`，同一工作区、同一 HOME、同一 PATH。
    std::fs::create_dir_all(roots.project_skills()).unwrap();
    let b = run_guarded(&["-p", "hi"], &roots);
    let (b_stdout, b_stderr) = streams(&b);

    // 1. 两次都不该有 skill 诊断。
    assert!(
        skill_markers_in(&a_stderr).is_empty(),
        "没有 skills 目录时不得有 skill 诊断，命中 {:?}；stderr:\n{a_stderr}",
        skill_markers_in(&a_stderr)
    );
    assert!(
        skill_markers_in(&b_stderr).is_empty(),
        "空 skills 目录时不得有 skill 诊断，命中 {:?}；stderr:\n{b_stderr}",
        skill_markers_in(&b_stderr)
    );

    // 2. stdout 皆空（诊断同样不得改道）。
    assert!(a_stdout.is_empty(), "stdout 应为空；stdout:\n{a_stdout:?}");
    assert!(b_stdout.is_empty(), "stdout 应为空；stdout:\n{b_stdout:?}");

    // 3. 「行为不变」：两次 stderr 逐字节相同（早退提示也一并比对）。
    assert_eq!(
        a_stderr, b_stderr,
        "「空 skills 目录」必须与「没有 skills 目录」行为完全一致"
    );
    assert!(
        a_stderr.contains("No model configured"),
        "两次都应走到早退（否则比对的是别的东西）；stderr:\n{a_stderr}"
    );
    assert_eq!(a.status.success(), b.status.success(), "两次退出状态应一致");

    roots.cleanup();
}
