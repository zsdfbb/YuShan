//! 后台命令执行（`bash_bg` → `bash_poll`）的集成验收。
//!
//! 被测的是**真实进程链路**：直接构造 [`BackgroundTasks`] + [`BashBgTool`] /
//! [`BashPollTool`]，在临时目录里跑完整链路 —— 启动、真进程、增量交付、退码、
//! 进程组清理。**不需要模型、不需要凭证、不引新依赖。**
//!
//! # 覆盖的契约
//!
//! | 用例 | 契约 |
//! |---|---|
//! | `starting_a_background_command_returns_a_handle_before_it_finishes` | 启动**不等命令结束** |
//! | `polling_delivers_only_the_bytes_that_were_not_delivered_yet` | 增量交付 + **不重复交付** |
//! | `polling_after_the_command_finished_reports_the_exit_code` | 终态给**退码** |
//! | `a_non_zero_exit_code_is_reported_as_an_error` | 退码 ≠ 0 ⇒ `is_error == true` |
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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::time::sleep;
use ys_core::ToolResult;
use ys_tool::{Tool, ToolContext};
use ys_tools_basic::{BashBgTool, BashPollTool, BackgroundTasks, BgLimits, SessionSlot};

/// 临时目录名的进程内序号 —— 与 `process::id()` 一起保证并行不撞名。
static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

/// 造出远超展示预算（50 KiB / 2000 行）的输出 —— 每行 71 字节、共 3000 行。
const LOUD_COMMAND: &str =
    "yes 0123456789012345678901234567890123456789012345678901234567890123456789 | head -3000";

/// 单个用例的墙钟上限 —— 全部等待都必须有界，测试不许挂死。
const CASE_LIMIT: Duration = Duration::from_secs(40);

/// 启动后观察「进程是否还在跑」的最长时间。
const GONE_LIMIT: Duration = Duration::from_secs(10);

/// 一个后台命令的测试环境 —— 注册表、两个工具、临时目录。
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
    /// 轮询工具。
    poll: BashPollTool,
}

impl Fixture {
    /// 搭一套环境 —— 注册表额度用 [`BgLimits::default`]。
    fn new(prefix: &str) -> Self {
        let seq = DIR_SEQ.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "ys_bg_test_{prefix}_{}_{seq}",
            std::process::id()
        ));
        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).expect("创建临时工作目录失败");
        let session = format!("sess-{seq}");
        let backgrounds = Arc::new(BackgroundTasks::new(root.clone(), BgLimits::default()));
        let slot = SessionSlot::new(session.clone());
        let bg = BashBgTool::new(backgrounds.clone(), workspace.clone(), slot.clone());
        let poll = BashPollTool::new(backgrounds.clone(), slot.clone());
        Self {
            root,
            workspace,
            session,
            backgrounds,
            bg,
            poll,
        }
    }

    /// 工具上下文 —— 无边界源（后台命令不走轮边界）。
    fn ctx(&self) -> ToolContext<'static> {
        ToolContext::new(None, self.workspace.clone(), self.workspace.clone())
    }

    /// 启动一条后台命令，返回 `(句柄, 启动结果文本)`。
    async fn start(&self, command: &str) -> (String, String) {
        let result = guarded(
            "bash_bg 调用",
            CASE_LIMIT,
            self.bg.call(json!({ "command": command }), self.ctx()),
        )
        .await
        .expect("bash_bg 应成功返回");
        assert!(!result.is_error, "启动不应是错误：{}", result.content);
        let handle = field(&result.content, "handle").expect("结果里应有句柄");
        (handle, result.content)
    }

    /// 轮询一条后台命令。
    async fn poll(&self, handle: &str, wait_seconds: u64) -> ToolResult {
        guarded(
            "bash_poll 调用",
            CASE_LIMIT,
            self.poll
                .call(json!({ "handle": handle, "wait_seconds": wait_seconds }), self.ctx()),
        )
        .await
        .expect("bash_poll 应成功返回")
    }

    /// 某个句柄的输出文件路径。
    fn output_path(&self, handle: &str) -> PathBuf {
        self.root
            .join(&self.session)
            .join(format!("{handle}.log"))
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
    content
        .lines()
        .find_map(|line| line.strip_prefix(&prefix).map(|value| value.trim().to_string()))
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
            panic!("输出文件在 {limit:?} 内没有出现 {needle:?}：{}", path.display());
        }
        sleep(Duration::from_millis(25)).await;
    }
}

/// 启动**不等命令结束**就返回句柄。
#[tokio::test]
async fn starting_a_background_command_returns_a_handle_before_it_finishes() {
    let fixture = Fixture::new("start");
    let started = Instant::now();
    let (handle, content) = fixture.start("sleep 5; echo done").await;
    let elapsed = started.elapsed();

    assert!(handle.starts_with("bg"), "句柄形态应是 bgN：{handle}");
    assert!(
        elapsed < Duration::from_secs(3),
        "启动**不等命令结束**（命令要跑 5s），实测 {elapsed:?}"
    );
    assert!(content.contains("pid:"), "结果里应有 pid：{content}");
    assert!(
        content.contains("output:"),
        "结果里应有输出文件路径：{content}"
    );
}

/// 轮询只交付**尚未交付**的字节 —— 第二次轮询不重复，第三次没有新字节。
#[tokio::test]
async fn polling_delivers_only_the_bytes_that_were_not_delivered_yet() {
    let fixture = Fixture::new("incremental");
    let (handle, _) = fixture.start("echo alpha; sleep 3; echo beta").await;
    let output_path = fixture.output_path(&handle);

    // 等第一段真的落盘 —— 不靠睡固定时长赌时序。
    wait_for_output(&output_path, "alpha", CASE_LIMIT).await;

    let first = fixture.poll(&handle, 0).await;
    assert!(first.content.contains("alpha"), "第一次轮询应拿到增量：{}", first.content);
    assert!(
        !first.content.contains("beta"),
        "还没产生的输出不该出现：{}",
        first.content
    );

    // 第二次轮询：等命令结束 ⇒ 拿到第二段，且**不重复**第一段。
    let second = fixture.poll(&handle, 15).await;
    assert!(second.content.contains("beta"), "第二次轮询应拿到新段：{}", second.content);
    assert!(
        !second.content.contains("alpha"),
        "已送达的字节不得重复送达：{}",
        second.content
    );

    // 第三次：文件已到尾 ⇒ 没有新字节。
    let third = fixture.poll(&handle, 0).await;
    assert!(
        third.content.contains("0 B new"),
        "已交付完后再轮询应是 0 字节新：{}",
        third.content
    );
    assert!(
        !third.content.contains("alpha") && !third.content.contains("beta"),
        "已送达的字节不得重复送达：{}",
        third.content
    );
}

/// 命令结束后轮询拿到**退码**。
#[tokio::test]
async fn polling_after_the_command_finished_reports_the_exit_code() {
    let fixture = Fixture::new("exitcode");
    let (handle, _) = fixture.start("echo hi; exit 0").await;

    let result = fixture.poll(&handle, 15).await;
    assert!(
        result.content.contains("exit code 0"),
        "应报出退码 0：{}",
        result.content
    );
    assert!(!result.is_error, "退码 0 不算错误：{}", result.content);
}

/// 退码 ≠ 0 ⇒ 结果是**错误**。
#[tokio::test]
async fn a_non_zero_exit_code_is_reported_as_an_error() {
    let fixture = Fixture::new("nonzero");
    let (handle, _) = fixture.start("echo boom; exit 3").await;

    let result = fixture.poll(&handle, 15).await;
    assert!(result.is_error, "退码 3 应算错误：{}", result.content);
    assert!(
        result.content.contains("exit code 3"),
        "应报出退码 3：{}",
        result.content
    );
}

/// 清理会话后**没有进程还在跑**。
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
    // 句柄仍在注册表里，但条目已是终态。
    assert!(
        fixture
            .backgrounds
            .running_handles(&fixture.session)
            .is_empty(),
        "清理后不该还有在跑的句柄"
    );
    assert!(!handle.is_empty());
}

/// 输出超限 ⇒ 结果里报出**跳过的字节数**与**完整输出路径**。
#[tokio::test]
async fn oversized_output_reports_the_skipped_bytes_and_the_full_output_path() {
    let fixture = Fixture::new("trim");
    // 3000 行 × 71 字节 ≈ 208 KiB ⇒ 超过展示预算（50 KiB / 2000 行）。
    let (handle, content) = fixture.start(LOUD_COMMAND).await;
    let output_path = field(&content, "output").expect("结果里应有输出路径");

    let result = fixture.poll(&handle, 20).await;
    let produced = std::fs::metadata(&output_path).expect("输出文件应存在").len();
    assert!(
        produced > 100 * 1024,
        "产出应远超展示预算，实测 {produced} 字节"
    );
    assert!(
        result.content.len() < produced as usize,
        "展示内容应被裁剪过：结果 {} 字节 vs 产出 {produced} 字节",
        result.content.len()
    );
    assert!(
        result.content.contains(&output_path),
        "结果里应给出完整输出路径：{}",
        result.content
    );
    assert!(
        result.content.contains("未送达") || result.content.contains("skipped"),
        "结果里应报出未送达的字节数：{}",
        result.content
    );
}

/// 跨会话的句柄**不命中** —— 表现为「未知句柄」。
#[tokio::test]
async fn a_handle_from_another_session_is_reported_as_unknown() {
    let fixture = Fixture::new("crosssession");
    let (handle, _) = fixture.start("echo secret; sleep 5").await;

    // 另一个会话的槽 ⇒ 同一条注册表，但会话不匹配。
    let other_slot = SessionSlot::new("sess-other");
    let other_poll = BashPollTool::new(fixture.backgrounds.clone(), other_slot);
    let result = guarded(
        "跨会话轮询",
        CASE_LIMIT,
        other_poll.call(json!({ "handle": handle }), fixture.ctx()),
    )
    .await
    .expect("跨会话轮询应返回结果而不是报错");

    assert!(result.is_error, "跨会话句柄应是错误：{}", result.content);
    assert!(
        result.content.contains("unknown handle"),
        "跨会话句柄应是「未知句柄」：{}",
        result.content
    );
    assert!(
        !result.content.contains("secret"),
        "跨会话不得泄漏别的会话的输出：{}",
        result.content
    );
}

/// 内核侧的写盘限额是**粗闸门** ⇒ 正常写几 MiB 的命令**不被杀**。
#[tokio::test]
async fn a_command_that_writes_several_megabytes_to_disk_is_not_killed() {
    let fixture = Fixture::new("bigwrite");
    let (handle, _) = fixture
        .start("dd if=/dev/zero of=build.bin bs=1024 count=4096; echo wrote")
        .await;

    let result = fixture.poll(&handle, 20).await;
    assert!(
        !result.is_error,
        "正常写盘不该被内核限额杀掉：{}",
        result.content
    );
    assert!(
        result.content.contains("exit code 0"),
        "应正常跑完：{}",
        result.content
    );
    assert!(result.content.contains("wrote"), "应拿到命令输出：{}", result.content);
    let written = std::fs::metadata(fixture.workspace.join("build.bin"))
        .expect("落盘文件应存在")
        .len();
    assert!(written >= 4 * 1024 * 1024, "落盘应完整，实测 {written} 字节");
}

/// 杀进程组 ⇒ **孙进程也不在**（先杀后收 + 进程组）。
#[tokio::test]
async fn killing_the_command_group_leaves_no_orphan_grandchild() {
    let fixture = Fixture::new("grandchild");
    let (handle, content) = fixture.start("sleep 60 & echo $! > grandchild.pid; sleep 60").await;
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
    assert!(!handle.is_empty());
}
