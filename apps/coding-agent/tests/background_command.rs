//! 后台命令执行（`bash_bg` → 完成时**自动注入**）的集成验收。
//!
//! 被测的是**真实进程链路**：直接构造 [`BackgroundTasks`] + [`BashBgTool`]，在临时目录里
//! 跑完整链路 —— 启动、真进程、收尸、退码、正文渲染、进程组清理。**不需要模型、不需要凭证、
//! 不引新依赖。**
//!
//! # 结果的取回方式
//!
//! 模型面**没有轮询工具**：命令结束时由 sweeper 把结果 `push` 成 `Boundary::Deferred`，
//! 在下一个轮边界自动进会话。故本文件取结果的路径是**生产路径**的两步 ——
//! [`BackgroundTasks::snapshot`]（等它离开 `Running`）+
//! [`BackgroundTasks::take_completions`]（认领完成项）—— 然后交给**唯一渲染点**
//! [`render_completion`] 断言正文。
//!
//! 这里**不驱动 sweeper 与边界源**（那两条由 `app_loop.rs` 的单元测试负责，见
//! `the_shared_sweeper_reaps_finished_commands`）：本文件验的是「真进程 → 完成项 → 正文」
//! 这一段，投递的恰好一次 / 未绑定留表 / 补投限流都属于注册表与接线器的职责范围。
//!
//! # 覆盖的契约
//!
//! | 用例 | 契约 |
//! |---|---|
//! | `starting_a_background_command_returns_a_handle_before_it_finishes` | 启动**不等命令结束** |
//! | `the_completion_carries_the_real_output_and_the_exit_code` | 终态给**退码** + 真输出 + 完整路径 |
//! | `a_non_zero_exit_code_is_reported_in_the_completion_body` | 退码 ≠ 0 **写在正文里**（消息没有 error 位） |
//! | `a_second_start_gets_a_fresh_handle` | 句柄**不复用** |
//! | `cleaning_up_the_session_leaves_no_running_process` | 清理后**进程数 = 0** |
//! | `oversized_output_reports_the_skipped_bytes_and_the_full_output_path` | 裁剪**报出**跳过量与完整路径 |
//! | `a_handle_from_another_session_is_reported_as_unknown` | **跨会话句柄不命中** |
//! | `a_command_that_writes_several_megabytes_to_disk_is_not_killed` | 内核粗闸门**不误杀**正常写盘 |
//! | `killing_the_command_group_leaves_no_orphan_grandchild` | 杀组后**孙进程也不在** |
//!
//! # 隔离
//!
//! 临时目录名含**进程内原子序号 + `process::id()`**（`SystemTime::as_nanos()` 在
//! macOS 只到微秒级，并行测试会撞名 ⇒ 互删文件）。**不改写进程级环境变量** ——
//! 落盘根目录直接传给 [`BackgroundTasks::new`]，不碰 `YUSHAN_BG_DIR`。
//!
//! # 收尾
//!
//! [`Fixture`] 的 `Drop` 会杀掉本会话的**全部**后台进程组并删除临时目录 ⇒
//! 即使断言 panic 也不会留下孤儿进程或残留目录。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::time::sleep;
use ys_core::Message;
use ys_tool::{Tool, ToolContext};
use ys_tools_basic::{
    BackgroundTasks, BashBgTool, BgCompletion, BgHandle, BgLimits, BgState, SessionSlot,
    render_completion,
};

/// 临时目录名的进程内序号 —— 与 `process::id()` 一起保证并行不撞名。
static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

/// 造出远超展示预算（50 KiB / 2000 行）的输出 —— 每行 71 字节、共 3000 行。
const LOUD_COMMAND: &str =
    "yes 0123456789012345678901234567890123456789012345678901234567890123456789 | head -3000";

/// 单个用例的墙钟上限 —— 全部等待都必须有界，测试不许挂死。
const CASE_LIMIT: Duration = Duration::from_secs(40);

/// 启动后观察「进程是否还在跑」的最长时间。
const GONE_LIMIT: Duration = Duration::from_secs(10);

/// 一个后台命令的测试环境 —— 注册表、启动工具、临时目录。
struct Fixture {
    /// 落盘根目录（注册表的 root）。
    root: PathBuf,
    /// 工作目录（命令的 cwd）。
    workspace: PathBuf,
    /// 本注册表内使用的会话 id。
    session: String,
    /// 进程级注册表。
    backgrounds: Arc<BackgroundTasks>,
    /// 启动工具。
    bg: BashBgTool,
}

impl Fixture {
    /// 搭一套环境 —— 注册表额度用 [`BgLimits::default`]。
    fn new(prefix: &str) -> Self {
        let seq = DIR_SEQ.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("ys_bg_test_{prefix}_{}_{seq}", std::process::id()));
        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).expect("创建临时工作目录失败");
        let session = format!("sess-{seq}");
        let backgrounds = Arc::new(BackgroundTasks::new(root.clone(), BgLimits::default()));
        let slot = SessionSlot::new(session.clone());
        let bg = BashBgTool::new(backgrounds.clone(), workspace.clone(), slot);
        Self {
            root,
            workspace,
            session,
            backgrounds,
            bg,
        }
    }

    /// 工具上下文 —— 无边界源（后台命令不走轮边界）。
    fn ctx(&self) -> ToolContext<'static> {
        ToolContext::new(None, self.workspace.clone(), self.workspace.clone())
    }

    /// 启动一条后台命令，返回 `(句柄, 启动结果文本)`。
    async fn start(&self, command: &str) -> (BgHandle, String) {
        let result = guarded(
            "bash_bg 调用",
            CASE_LIMIT,
            self.bg.call(json!({ "command": command }), self.ctx()),
        )
        .await
        .expect("bash_bg 应成功返回");
        assert!(!result.is_error, "启动不应是错误：{}", result.content);
        let label = field(&result.content, "handle").expect("结果里应有句柄");
        (handle_of(&label), result.content)
    }

    /// 等一条命令离开 `Running`，再**按生产路径**认领它的完成项。
    ///
    /// 等待用 [`BackgroundTasks::snapshot`]（它顺带收尸并执行输出上限闸门 ⇒ 与被测链路
    /// 同一口径）；认领用 [`BackgroundTasks::take_completions`]，取得后立刻
    /// [`BackgroundTasks::finish_delivery`] 落状态 —— 与 `app_loop` 的补投顺序一致。
    async fn completion(&self, handle: BgHandle) -> BgCompletion {
        let deadline = Instant::now() + CASE_LIMIT;
        loop {
            match self.backgrounds.snapshot(handle, &self.session) {
                Some(snapshot) if snapshot.state != BgState::Running => break,
                Some(_) => {}
                None => panic!("句柄 {handle:?} 已从注册表消失（被淘汰？）"),
            }
            assert!(
                Instant::now() < deadline,
                "命令 {handle:?} 在 {CASE_LIMIT:?} 内没有结束"
            );
            sleep(Duration::from_millis(20)).await;
        }

        let mut claimed = self.backgrounds.take_completions(&self.session);
        let index = claimed
            .iter()
            .position(|candidate| candidate.handle == handle)
            .unwrap_or_else(|| panic!("{handle:?} 已是终态，却取不到完成项"));
        let candidate = claimed.remove(index);
        self.backgrounds.finish_delivery(handle, true);
        candidate
    }

    /// 某条命令的**模型可见正文**（首行 + 正文拼成一段）。
    async fn rendered(&self, handle: BgHandle) -> String {
        text_of(&render_completion(&self.completion(handle).await))
    }
}

impl Drop for Fixture {
    /// 收尾 —— 杀掉本会话的全部后台进程组，再删临时目录。
    fn drop(&mut self) {
        self.backgrounds.kill_all(&self.session);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// 有界等待 —— 超时即 panic（带标签），不让测试挂死。
async fn guarded<T>(label: &str, limit: Duration, future: impl Future<Output = T>) -> T {
    match tokio::time::timeout(limit, future).await {
        Ok(value) => value,
        Err(_) => panic!("{label}：超过 {limit:?} 仍未完成"),
    }
}

/// 从工具结果文本里取 `key: value` 形态的字段。
fn field(content: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}: ");
    content.lines().find_map(|line| {
        line.strip_prefix(&prefix)
            .map(|value| value.trim().to_string())
    })
}

/// `bg7` ⇒ `BgHandle(7)`（句柄号是模型可见的那部分，故从文案反解）。
fn handle_of(label: &str) -> BgHandle {
    let digits = label
        .strip_prefix("bg")
        .unwrap_or_else(|| panic!("句柄形态应是 bgN：{label}"));
    BgHandle(digits.parse().expect("句柄号应是数字"))
}

/// 一条消息的正文（`content` 文本块拼起来）。
fn text_of(message: &Message) -> String {
    message
        .content
        .iter()
        .map(|block| match block {
            ys_core::ContentBlock::Text { text } => text.clone(),
            other => panic!("完成正文只应有文本块，得到 {other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 进程是否**还在跑** —— 僵尸态（`Z`）已经不在执行，不算「还在跑」。
fn process_running(pid: u32) -> bool {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else {
        // 探测失败 ⇒ 保守当作「还在跑」（宁可误报，不可漏报）。
        return true;
    };
    if !output.status.success() {
        // `ps` 找不到该进程 ⇒ 已经没了。
        return false;
    }
    let stat = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stat.is_empty() {
        return false;
    }
    !stat.starts_with('Z')
}

/// 等进程消失（有界）—— 返回是否真的消失了。
async fn wait_until_gone(pid: u32, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if !process_running(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

/// 等输出文件里出现某个片段（有界）—— 返回当时的文件内容。
async fn wait_for_output(path: &Path, needle: &str, limit: Duration) -> String {
    let deadline = Instant::now() + limit;
    loop {
        if let Ok(text) = tokio::fs::read_to_string(path).await
            && text.contains(needle)
        {
            return text;
        }
        if Instant::now() >= deadline {
            panic!(
                "输出文件在 {limit:?} 内没有出现 {needle:?}：{}",
                path.display()
            );
        }
        sleep(Duration::from_millis(25)).await;
    }
}

/// 启动**不等命令结束**就返回句柄。
///
/// 变异：把 `start` 改成等到进程退出再返回 ⇒ 第二条断言（实测耗时 < 4s）变红。
///
/// 阈值必须严格小于被测命令时长（5s），才能证明「没有等命令结束」；
/// 同时要留出并行测试负载下的调度余量（实测曾到 3.05s），故取 4s。
#[tokio::test]
async fn starting_a_background_command_returns_a_handle_before_it_finishes() {
    let fixture = Fixture::new("start");
    let started = Instant::now();
    let (handle, content) = fixture.start("sleep 5; echo done").await;
    let elapsed = started.elapsed();

    assert_eq!(handle, BgHandle(1), "首个句柄应是 bg1");
    assert!(
        elapsed < Duration::from_secs(4),
        "启动**不等命令结束**（命令要跑 5s），实测 {elapsed:?}"
    );
    assert!(content.contains("pid:"), "结果里应有 pid：{content}");
    assert!(
        content.contains("output:"),
        "结果里应有输出文件路径：{content}"
    );
    assert!(
        content.contains("do NOT poll for it"),
        "模型面无轮询工具 ⇒ 文案必须禁止轮询：{content}"
    );
}

/// 终态正文给出**退码**、**真输出**、**完整输出路径**。
///
/// 变异①：`render_completion` 不读输出文件（正文留空）⇒ 「含真输出」断言变红。
/// 变异②：`reap` 不落 `Exited(Some(code))` ⇒ 首行变成「被信号杀掉」⇒ 退码断言变红。
#[tokio::test]
async fn the_completion_carries_the_real_output_and_the_exit_code() {
    let fixture = Fixture::new("output");
    let (handle, _) = fixture.start("echo hi-from-bg; exit 0").await;

    let completion = fixture.completion(handle).await;
    assert_eq!(
        completion.state,
        BgState::Exited(Some(0)),
        "正常结束应落 Exited(Some(0))"
    );

    let body = text_of(&render_completion(&completion));
    assert!(
        body.lines()
            .next()
            .unwrap_or_default()
            .contains("finished (exit 0)"),
        "首行必须报出退码：{body}"
    );
    assert!(
        body.contains("hi-from-bg"),
        "正文必须含命令的真输出：{body}"
    );
    assert!(
        body.contains(&format!(
            "full output: {}",
            completion.output_path.display()
        )),
        "正文必须给出完整输出路径：{body}"
    );
}

/// 退码 ≠ 0 也必须**写在正文里** —— 完成项是注入会话的一条 `User` 消息，
/// **没有 `is_error` 位** ⇒ 「失败」这条信息只能由措辞携带。
///
/// 变异：`render_completion` 只写「finished」不写退码 ⇒ 第二条断言变红。
#[tokio::test]
async fn a_non_zero_exit_code_is_reported_in_the_completion_body() {
    let fixture = Fixture::new("nonzero");
    let (handle, _) = fixture.start("echo boom; exit 3").await;

    let completion = fixture.completion(handle).await;
    assert_eq!(completion.state, BgState::Exited(Some(3)));

    let body = text_of(&render_completion(&completion));
    assert!(
        body.lines()
            .next()
            .unwrap_or_default()
            .contains("finished (exit 3)"),
        "退码必须写进首行：{body}"
    );
    assert!(body.contains("boom"), "正文必须含命令的真输出：{body}");
}

/// 句柄**不复用**：第一条结束后再启动，拿到的是新号。
///
/// 变异：把句柄号改成「复用最小空闲号」⇒ 第二条断言变红（会拿到 bg1）。
#[tokio::test]
async fn a_second_start_gets_a_fresh_handle() {
    let fixture = Fixture::new("reuse");
    let (first, _) = fixture.start("true").await;
    fixture.completion(first).await;

    let (second, content) = fixture.start("true").await;
    assert_ne!(first, second, "句柄不得复用，实得 {first:?} / {second:?}");
    assert_eq!(second, BgHandle(2), "句柄号应单调递增");
    assert!(
        content.contains("handle: bg2"),
        "启动结果里的句柄名应与注册表一致：{content}"
    );
}

/// 清理会话后**没有进程还在跑**。
///
/// 变异：`kill_all` 去掉会话之外的过滤或干脆不杀 ⇒ 探活断言变红。
#[tokio::test]
async fn cleaning_up_the_session_leaves_no_running_process() {
    let fixture = Fixture::new("cleanup");
    let (handle, content) = fixture.start("sleep 60").await;
    let pid: u32 = field(&content, "pid")
        .expect("结果里应有 pid")
        .parse()
        .expect("pid 应是数字");
    assert!(process_running(pid), "命令此刻应在跑");

    fixture.backgrounds.kill_all(&fixture.session);

    assert!(
        wait_until_gone(pid, GONE_LIMIT).await,
        "清理后 pid {pid} 仍在跑"
    );
    // 条目仍在注册表里，但已不在跑。
    assert!(
        fixture
            .backgrounds
            .running_handles(&fixture.session)
            .is_empty(),
        "清理后不该还有在跑的句柄"
    );
    assert_eq!(handle, BgHandle(1));
}

/// 输出超限 ⇒ 正文里报出**跳过的字节数**与**完整输出路径**。
///
/// 变异：`assemble` 不报裁量、不拼路径 ⇒ 后两条断言变红。
#[tokio::test]
async fn oversized_output_reports_the_skipped_bytes_and_the_full_output_path() {
    let fixture = Fixture::new("trim");
    // 3000 行 × 71 字节 ≈ 208 KiB ⇒ 超过展示预算（50 KiB / 2000 行）。
    let (handle, content) = fixture.start(LOUD_COMMAND).await;
    let output_path = PathBuf::from(field(&content, "output").expect("结果里应有输出路径"));

    let body = fixture.rendered(handle).await;
    let produced = std::fs::metadata(&output_path)
        .expect("输出文件应存在")
        .len();
    assert!(
        produced > 100 * 1024,
        "产出应远超展示预算，实测 {produced} 字节"
    );
    assert!(
        body.len() < produced as usize,
        "展示内容应被裁剪过：正文 {} 字节 vs 产出 {produced} 字节",
        body.len()
    );
    assert!(
        body.contains(&format!("full output: {}", output_path.display())),
        "正文里应给出完整输出路径：{body}"
    );
    assert!(
        body.contains("未送达"),
        "正文里应报出未送达的字节数：{body}"
    );
}

/// 跨会话的句柄**不命中** —— 表现为「未知句柄」。
///
/// 迁移后模型面没有取状态工具，故这条契约落在**注册表的会话核对**上
/// （`app_loop` 用它把「未知句柄」讲给模型）。
///
/// 变异：`snapshot` / `claim_completions` 去掉会话条件 ⇒ 前两条断言变红。
#[tokio::test]
async fn a_handle_from_another_session_is_reported_as_unknown() {
    let fixture = Fixture::new("crosssession");
    let (handle, _) = fixture.start("echo secret; sleep 60").await;

    // 另一个会话 ⇒ 同一份注册表，但会话不匹配。
    assert!(
        fixture.backgrounds.snapshot(handle, "sess-other").is_none(),
        "跨会话的句柄不得命中（对模型表现为未知句柄）"
    );
    assert!(
        fixture
            .backgrounds
            .take_completions("sess-other")
            .is_empty(),
        "跨会话取不到任何完成项（否则会泄漏别的会话的输出）"
    );
    assert!(
        !fixture.root.join("sess-other").exists(),
        "别的会话不该被写下输出目录"
    );

    // 本会话照旧命中 —— 否则上一条断言可能是「什么都没写」而非「会话过滤生效」。
    assert!(
        fixture
            .backgrounds
            .snapshot(handle, &fixture.session)
            .is_some(),
        "同一会话必须命中"
    );
}

/// 内核侧的写盘限额是**粗闸门** ⇒ 正常写几 MiB 的命令**不被杀**。
///
/// 变异：把内核限额设成输出上限 ⇒ 命令吃 `SIGXFSZ` ⇒ 退码与落盘大小两条断言变红。
#[tokio::test]
async fn a_command_that_writes_several_megabytes_to_disk_is_not_killed() {
    let fixture = Fixture::new("bigwrite");
    let (handle, _) = fixture
        .start("dd if=/dev/zero of=build.bin bs=1024 count=4096 2>/dev/null; echo wrote")
        .await;

    let completion = fixture.completion(handle).await;
    assert_eq!(
        completion.state,
        BgState::Exited(Some(0)),
        "正常写盘不该被内核限额杀掉"
    );
    let body = text_of(&render_completion(&completion));
    assert!(body.contains("wrote"), "应拿到命令输出：{body}");

    let written = std::fs::metadata(fixture.workspace.join("build.bin"))
        .expect("落盘文件应存在")
        .len();
    assert!(
        written >= 4 * 1024 * 1024,
        "落盘应完整，实测 {written} 字节"
    );
}

/// 杀进程组 ⇒ **孙进程也不在**（先杀后收 + 进程组）。
///
/// 变异：`kill_group` 只杀 leader（不带 `-pid` 进程组）⇒ 孙进程探活断言变红。
#[tokio::test]
async fn killing_the_command_group_leaves_no_orphan_grandchild() {
    let fixture = Fixture::new("grandchild");
    let (_, content) = fixture
        .start("sleep 60 & echo $! > grandchild.pid; sleep 60")
        .await;
    let leader: u32 = field(&content, "pid")
        .expect("结果里应有 pid")
        .parse()
        .expect("pid 应是数字");

    // 孙进程的 pid 由命令自己写下来。
    let pid_file = fixture.workspace.join("grandchild.pid");
    let deadline = Instant::now() + CASE_LIMIT;
    let grandchild = loop {
        if let Ok(text) = tokio::fs::read_to_string(&pid_file).await
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            break pid;
        }
        assert!(
            Instant::now() < deadline,
            "孙进程 pid 文件在 {CASE_LIMIT:?} 内没出现"
        );
        sleep(Duration::from_millis(25)).await;
    };
    assert!(process_running(grandchild), "孙进程此刻应在跑");

    fixture.backgrounds.kill_all(&fixture.session);

    assert!(
        wait_until_gone(leader, GONE_LIMIT).await,
        "清理后 leader pid {leader} 仍在跑"
    );
    assert!(
        wait_until_gone(grandchild, GONE_LIMIT).await,
        "清理后孙进程 pid {grandchild} 仍在跑 —— 进程组没被杀干净"
    );
}

/// 慢速写输出的命令在**跑着的时候**就被输出闸门杀掉 —— 顺带证明
/// `wait_for_output` 观察到的中间态**不是**终态（因此下面那条断言才有意义）。
///
/// 变异：把 `enforce_output_cap` 的 `stat` 闸门去掉（只留内核粗闸门）⇒ 输出
/// 会一路写到内核限额（64 MiB）才被杀 ⇒ 本用例的「文件停在 2 MiB 量级」断言变红。
#[tokio::test]
async fn the_output_cap_kills_a_runaway_command_while_it_is_still_writing() {
    let fixture = Fixture::new("cap");
    // 每 1s 写 1 MiB、共 8 块 ⇒ 远慢于闸门节奏（20ms 一查）⇒ 一定在「还在写」时被抓到；
    // 而全部写完是 8 MiB ⇒ 被闸门兜住的证据是「文件停在 2 MiB 量级」。
    let (handle, content) = fixture
        .start(
            "i=0; while [ \"$i\" -lt 8 ]; do \
             dd if=/dev/zero bs=1048576 count=1 2>/dev/null | tr '\\000' 'x'; \
             sleep 1; i=$((i+1)); done",
        )
        .await;
    let output_path = PathBuf::from(field(&content, "output").expect("结果里应有输出路径"));

    // 先等它真的写出点东西 —— 证明命令在跑而不是立刻失败。
    wait_for_output(&output_path, "xxxx", CASE_LIMIT).await;

    let completion = fixture.completion(handle).await;
    assert_eq!(
        completion.state,
        BgState::Killed,
        "输出超限必须被 harness 杀掉（不是静默截断，也不是正常结束）"
    );

    // 「被杀」与「命令自己失败」必须用不同措辞 —— 模型会把前者误报成「测试挂了」。
    let body = text_of(&render_completion(&completion));
    assert!(
        body.lines()
            .next()
            .unwrap_or_default()
            .contains("output exceeded"),
        "首行必须说清是输出超限被杀：{body}"
    );

    // 闸门是「轮询式」的 ⇒ 杀到之前可能又落了一块（1 MiB）⇒ 上界留一块的余量。
    let size = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    assert!(
        (2 * 1024 * 1024..=4 * 1024 * 1024).contains(&size),
        "闸门必须把输出兜在 2 MiB 附近（而不是任它写到 8 MiB），实测 {size} 字节"
    );
}
