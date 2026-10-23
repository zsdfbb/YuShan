//! 后台命令的启动工具 —— `bash_bg`。
//!
//! **不准并行**（[`Tool::is_concurrency_safe`] 恒 `false`）：启动会 spawn 进程 ⇒ 有副作用。
//!
//! **cwd 契约**：用**工具自带的 workspace**（与 `bash` 同一条），**不是** `ctx.cwd`
//! ⇒ 同批调用里的两条命令落在同一目录，模型不会误判位置。
//!
//! **输出是 stdout+stderr 的合并流** —— 既有 `bash` 是分开收再拼；后台形态下
//! 两者按写入顺序交错，模型无法区分来源 ⇒ 这是后台形态必须接受的差异。
//!
//! **结果由 harness 主动送达，模型不轮询**：命令结束时后台 sweeper 把结果投递成一条
//! `Boundary::Deferred`，在下一个轮边界自动进会话；回合先结束则留表，下一回合开头补投。
//! 故本工具的返回文案**明确要求模型不要轮询**，直接继续自己那部分工作。
//!
//! **启动失败同步报错** —— 绝不返回「永远就绪」的句柄，注册表里也不留条目。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use ys_core::ToolResult;
use ys_tool::{Tool, ToolContext, ToolError, ToolSpec};

use crate::background::{BackgroundTasks, BgError, SessionSlot};

/// `bash_bg` 的工具级墙钟上限 —— 启动本身只是几个同步 syscall（~1ms）。
const BG_TIMEOUT: Duration = Duration::from_secs(30);

/// `bash_bg` —— 启动一条后台命令并**立刻**返回句柄。
pub struct BashBgTool {
    backgrounds: Arc<BackgroundTasks>,
    /// 工作目录 —— **工具自带**，与 `bash` 同一条契约。
    workspace: PathBuf,
    /// 当前会话 id 的共享槽（`/new`、`/resume` 后由 app 层切换）。
    session: SessionSlot,
}

impl BashBgTool {
    /// 装配工具 —— `backgrounds` 与 `bash` 共享同一份注册表。
    pub fn new(
        backgrounds: Arc<BackgroundTasks>,
        workspace: PathBuf,
        session: SessionSlot,
    ) -> Self {
        Self {
            backgrounds,
            workspace,
            session,
        }
    }
}

#[async_trait::async_trait]
impl Tool for BashBgTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "bash_bg",
            "Start a shell command in the background and get a handle back immediately; the \
             command keeps running while you do other work. Its result is delivered to you \
             automatically when it finishes — do NOT poll for it, just keep working on your own \
             part. Use this for commands that take more than a few seconds (test suites, \
             builds); use bash for quick commands.",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" },
                    "timeout": { "type": "number", "description": "Max lifetime in seconds (default 900, max 3600)" }
                },
                "required": ["command"]
            }),
        )
    }

    /// 启动会 spawn 进程 ⇒ 有副作用 ⇒ **永不准开并行**。
    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn timeout(&self) -> Option<Duration> {
        Some(BG_TIMEOUT)
    }

    async fn call(&self, input: Value, _ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let command = input["command"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing 'command'".into()))?;
        if command.trim().is_empty() {
            return Err(ToolError::InvalidInput("'command' is empty".into()));
        }
        let timeout = input["timeout"].as_u64().map(Duration::from_secs);
        let session = self.session.get();

        // 启动失败**同步报错** —— 绝不返回「永远就绪」的句柄，注册表里也不留条目。
        let handle = self
            .backgrounds
            .start(&session, command, &self.workspace, timeout)
            .map_err(|error| match error {
                // **文案只指向模型真能做的动作** —— 模型面**没有 kill 入口**（本工具只能
                // start），模型能做的只有「等一条跑完」⇒ 额度一空就能再起一条。
                BgError::TooManyRunning { limit } => ToolError::Execution(format!(
                    "[bash_bg] too many background commands are running ({limit}) — \
                     wait for one to finish, then start another"
                )),
                // 表满 ⇒ 全部条目都在跑 ⇒ 同样只能等（腾出的条目由下次 start 淘汰）。
                BgError::TableFull { limit } => ToolError::Execution(format!(
                    "[bash_bg] background command table is full ({limit}) — \
                     wait for one to finish, then start another"
                )),
                BgError::Spawn { source, .. } => {
                    ToolError::Execution(format!("[bash_bg] failed to start command: {source}"))
                }
                other => {
                    ToolError::Execution(format!("[bash_bg] failed to start command: {other}"))
                }
            })?;

        let snapshot = self.backgrounds.snapshot(handle, &session).ok_or_else(|| {
            ToolError::Execution(format!(
                "[bash_bg] handle {} vanished right after start",
                handle.label()
            ))
        })?;

        // 文案断言**不要轮询**：结果是 harness 主动送达的，模型该继续自己那部分工作。
        let label = snapshot.handle.label();
        Ok(ToolResult {
            content: format!(
                "Started in background.\n\
                 handle: {label}\n\
                 pid: {}\n\
                 cwd: {}\n\
                 output: {}\n\
                 Its result will be delivered to you automatically when it finishes — do NOT \
                 poll for it.\n\
                 Keep working on your own part now. If this turn ends first, the result is \
                 delivered at the start of your next turn.",
                snapshot.pid,
                snapshot.cwd.display(),
                snapshot.output_path.display(),
            ),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Instant;

    use serde_json::json;

    use crate::background::{BgHandle, BgLimits, BgSnapshot, BgState};
    use crate::test_support::{ctx, test_dir};

    /// 造一套最小装配：注册表 + `bash_bg`。
    ///
    /// 返回 `(注册表, bash_bg, 落盘根目录)`。
    fn tools(tag: &str) -> (Arc<BackgroundTasks>, BashBgTool, PathBuf) {
        let root = test_dir(tag);
        let tasks = Arc::new(BackgroundTasks::new(root.clone(), BgLimits::default()));
        let workspace = std::env::temp_dir();
        let session = SessionSlot::new("session-a");
        let bg = BashBgTool::new(tasks.clone(), workspace, session);
        (tasks, bg, root)
    }

    /// 收尾：杀掉该会话的全部后台进程组并收尸，删掉落盘目录 —— 不留孤儿进程。
    fn cleanup(tasks: &BackgroundTasks, root: &Path) {
        tasks.kill_all("session-a");
        tasks.sweep();
        let _ = fs::remove_dir_all(root);
    }

    /// 等命令离开 `Running`（到点即返回当前状态，不挂死）。
    ///
    /// `snapshot` 顺带执行输出上限闸门 ⇒ 这条等待路径与生产侧的闸门口径一致。
    async fn wait_until_done(
        tasks: &BackgroundTasks,
        handle: BgHandle,
        limit: Duration,
    ) -> BgSnapshot {
        let deadline = Instant::now() + limit;
        loop {
            let snapshot = tasks.snapshot(handle, "session-a").expect("句柄应存在");
            if snapshot.state != BgState::Running || Instant::now() >= deadline {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// 产出一条**慢速**写 stdout 的命令 —— 速率约 `chunk_bytes` 每秒。
    ///
    /// 必须**慢**：输出闸门是**轮询式**的 ⇒ 写得快的命令可能在两次检查之间就
    /// 写完并退出 ⇒ 闸门来不及杀它。慢写才能确定「命令还在跑时被闸门杀掉」。
    fn slow_writer(chunk_bytes: u32, chunks: u32) -> String {
        format!(
            "i=0; while [ \"$i\" -lt {chunks} ]; do \
            dd if=/dev/zero bs={chunk_bytes} count=1 2>/dev/null | tr '\\000' 'x'; \
            sleep 1; i=$((i+1)); done"
        )
    }

    // ---------- spec 逐字 ----------

    /// `spec` **逐字**钉住 —— spec 是模型可见契约，改字即改契约。
    #[test]
    fn the_spec_matches_the_model_visible_contract() {
        let (_tasks, bg, _root) = tools("bg_spec");

        let spec = bg.spec();
        assert_eq!(spec.name, "bash_bg");
        assert_eq!(
            spec.description,
            "Start a shell command in the background and get a handle back immediately; the \
             command keeps running while you do other work. Its result is delivered to you \
             automatically when it finishes — do NOT poll for it, just keep working on your own \
             part. Use this for commands that take more than a few seconds (test suites, \
             builds); use bash for quick commands."
        );
        assert!(
            !spec.description.contains("bash_poll"),
            "模型面已没有轮询工具 ⇒ spec 不得指向它：{}",
            spec.description
        );
        assert_eq!(
            spec.parameters,
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" },
                    "timeout": { "type": "number", "description": "Max lifetime in seconds (default 900, max 3600)" }
                },
                "required": ["command"]
            })
        );
    }

    // ---------- 启动文案 ----------

    /// 启动结果必须**明确要求不要轮询**，并给出句柄 / pid / cwd / 输出路径。
    ///
    /// 变异：把文案改成「用某个工具去取」⇒ 本用例变红（模型会重新开始空轮询）。
    #[tokio::test]
    async fn the_start_result_tells_the_model_not_to_poll() {
        let (tasks, bg, root) = tools("bg_start_text");
        let result = bg
            .call(json!({ "command": "sleep 30" }), ctx())
            .await
            .unwrap();

        let content = &result.content;
        assert!(!result.is_error, "启动不是错误结果：{content}");
        assert!(
            content.starts_with("Started in background.\n"),
            "首行固定：{content}"
        );
        assert!(content.contains("handle: bg1"), "必须给出句柄：{content}");
        assert!(content.contains("pid: "), "必须给出 pid：{content}");
        assert!(
            content.contains(&format!("cwd: {}", std::env::temp_dir().display())),
            "必须给出 cwd：{content}"
        );
        let path = root.join("session-a").join("bg1.log");
        assert!(
            content.contains(&format!("output: {}", path.display())),
            "必须给出输出文件路径：{content}"
        );
        assert!(
            content.contains("Its result will be delivered to you automatically when it finishes"),
            "必须说明结果会自动送到：{content}"
        );
        assert!(
            content.contains("do NOT poll for it"),
            "必须明确禁止轮询：{content}"
        );
        assert!(
            content.contains("Keep working on your own part now"),
            "必须让模型继续自己那部分工作：{content}"
        );
        assert!(
            content.contains("the result is delivered at the start of your next turn"),
            "跨回合送达必须说清：{content}"
        );
        assert!(
            !content.contains("bash_poll"),
            "模型面已没有轮询工具 ⇒ 文案不得指向它：{content}"
        );

        cleanup(&tasks, &root);
    }

    /// `timeout()` 与并发标志 —— **不准开并行**。
    #[test]
    fn timeouts_and_concurrency_flags_match_the_contract() {
        let (_tasks, bg, _root) = tools("bg_flags");

        assert_eq!(bg.timeout(), Some(Duration::from_secs(30)));
        assert!(
            !bg.is_concurrency_safe(),
            "启动会 spawn 进程 ⇒ 有副作用 ⇒ 永不准开并行"
        );
    }

    // ---------- 错误路径 ----------

    /// 启动失败 ⇒ **同步报错**，且注册表**无新条目**。
    #[tokio::test]
    async fn a_failed_start_reports_an_error_and_leaves_no_entry() {
        let root = test_dir("bg_start_fail");
        let tasks = Arc::new(BackgroundTasks::new(root.clone(), BgLimits::default()));
        let session = SessionSlot::new("session-a");
        // 工作目录不存在 ⇒ spawn 失败
        let bg = BashBgTool::new(tasks.clone(), root.join("no-such-dir"), session);

        let result = bg.call(json!({ "command": "true" }), ctx()).await;
        let error = result.expect_err("工作目录不存在 ⇒ 启动必须同步报错");
        assert!(
            error.to_string().contains("failed to start command"),
            "got {error}"
        );
        assert_eq!(tasks.entry_count(), 0, "启动失败不得留下条目");

        let _ = fs::remove_dir_all(&root);
    }

    /// 超 `max_running` 的报错**只指向模型真能做的动作**。
    ///
    /// 模型面**没有 kill 入口**（本工具只能 start）⇒ 文案里出现「kill」就是
    /// 指一个**做不到**的动作 ⇒ 模型会被自己的报错锁死。
    #[tokio::test]
    async fn the_too_many_running_message_points_only_at_actions_the_model_can_take() {
        let root = test_dir("bg_max_running");
        let limits = BgLimits {
            max_running: 1,
            ..BgLimits::default()
        };
        let tasks = Arc::new(BackgroundTasks::new(root.clone(), limits));
        let session = SessionSlot::new("session-a");
        let bg = BashBgTool::new(tasks.clone(), std::env::temp_dir(), session);

        bg.call(json!({ "command": "sleep 30" }), ctx())
            .await
            .unwrap();
        let error = bg
            .call(json!({ "command": "true" }), ctx())
            .await
            .expect_err("超过 max_running 必须同步报错");
        let message = error.to_string();
        assert!(
            message.contains("too many background commands are running (1)"),
            "got {message}"
        );
        assert!(
            message.contains("wait for one to finish, then start another"),
            "文案必须指向模型真能做的动作（等一条跑完）：got {message}"
        );
        assert!(
            !message.contains("kill"),
            "模型面没有 kill 入口 ⇒ 文案不得指向 kill：got {message}"
        );

        cleanup(&tasks, &root);
    }

    /// 两条限额是**分开的**：`max_running` 数的是**在跑**的，已结束的条目**不占**名额；
    /// 已结束的条目只占**条目表**（`max_entries`）。
    #[tokio::test]
    async fn a_finished_entry_occupies_a_table_slot_but_not_a_running_slot() {
        let root = test_dir("bg_limits");
        let limits = BgLimits {
            max_running: 1,
            max_entries: 8,
            ..BgLimits::default()
        };
        let tasks = Arc::new(BackgroundTasks::new(root.clone(), limits));
        let session = SessionSlot::new("session-a");
        let bg = BashBgTool::new(tasks.clone(), std::env::temp_dir(), session);

        // 第一条跑完 ⇒ 终态 ⇒ 不占 `max_running` 名额（但仍占条目表）。
        bg.call(json!({ "command": "true" }), ctx()).await.unwrap();
        let finished = wait_until_done(&tasks, BgHandle(1), Duration::from_secs(5)).await;
        assert_eq!(
            finished.state,
            BgState::Exited(Some(0)),
            "第一条必须已正常结束"
        );
        assert_eq!(tasks.entry_count(), 1, "终态条目仍占条目表");
        assert!(
            tasks.running_handles("session-a").is_empty(),
            "终态条目不在跑 ⇒ 不占 max_running 名额"
        );

        // 唯一的名额空着 ⇒ 还能再起一条（在跑的）。
        bg.call(json!({ "command": "sleep 30" }), ctx())
            .await
            .unwrap();
        assert_eq!(
            tasks.running_handles("session-a"),
            vec![BgHandle(2)],
            "在跑的条目占着唯一的名额"
        );

        // 名额用满 ⇒ 报错（已结束的那条**不**算在内）。
        let error = bg
            .call(json!({ "command": "true" }), ctx())
            .await
            .expect_err("在跑的条数到上限必须同步报错");
        assert!(
            error
                .to_string()
                .contains("too many background commands are running (1)"),
            "got {error}"
        );

        cleanup(&tasks, &root);
    }

    // ---------- 输出闸门 ----------

    /// 输出上限闸门（**精确闸门**）：写爆输出的命令被**杀掉**，输出不会无限膨胀。
    #[tokio::test]
    async fn the_output_cap_stops_a_runaway_command() {
        let root = test_dir("bg_cap");
        let limits = BgLimits {
            max_output_bytes: 4096,
            ..BgLimits::default()
        };
        let tasks = Arc::new(BackgroundTasks::new(root.clone(), limits));
        let session = SessionSlot::new("session-a");
        let bg = BashBgTool::new(tasks.clone(), std::env::temp_dir(), session);

        bg.call(json!({ "command": slow_writer(64 * 1024, 64) }), ctx())
            .await
            .unwrap();
        let snapshot = wait_until_done(&tasks, BgHandle(1), Duration::from_secs(15)).await;

        assert_eq!(
            snapshot.state,
            BgState::Killed,
            "输出超限必须被 harness 杀掉（而不是静默截断）"
        );
        assert!(
            snapshot.output_bytes >= 4096 && snapshot.output_bytes <= 4096 + 256 * 1024,
            "输出闸门必须兜住，实际写了 {} 字节",
            snapshot.output_bytes
        );

        tasks.kill_all("session-a");
        tasks.sweep();
        let _ = fs::remove_dir_all(&root);
    }

    /// **A**：正常写大文件的命令**不被内核的文件大小限额杀掉** ——
    /// 后台命令的主要用途就是这类活（构建、打包、落盘）。
    ///
    /// 内核限额是**进程级**的 ⇒ 它一旦被设成输出上限，正常写 4 MiB 文件的命令
    /// 就会吃 `SIGXFSZ` 被杀掉 ⇒ 功能被打断。故内核限额必须**高于**输出上限。
    #[tokio::test]
    async fn a_command_writing_a_large_file_is_not_killed_by_the_kernel_limit() {
        let root = test_dir("bg_big_file");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let tasks = Arc::new(BackgroundTasks::new(root.join("bg"), BgLimits::default()));
        let session = SessionSlot::new("session-a");
        let bg = BashBgTool::new(tasks.clone(), workspace.clone(), session);

        bg.call(
            json!({ "command": "dd if=/dev/zero of=big.bin bs=1048576 count=4 2>/dev/null" }),
            ctx(),
        )
        .await
        .unwrap();
        let snapshot = wait_until_done(&tasks, BgHandle(1), Duration::from_secs(15)).await;

        assert_eq!(
            snapshot.state,
            BgState::Exited(Some(0)),
            "写大文件的命令必须正常跑完（内核限额不得打断它）"
        );
        let big = workspace.join("big.bin");
        let size = fs::metadata(&big).map(|meta| meta.len()).unwrap_or(0);
        assert_eq!(
            size,
            4 * 1024 * 1024,
            "文件必须完整写完（内核限额不得打断正常写盘）"
        );

        tasks.kill_all("session-a");
        tasks.sweep();
        let _ = fs::remove_dir_all(&root);
    }

    // ---------- 组装的**单一真源** ----------

    /// **单一真源**的守卫：本模块（非测试部分）不得出现输出拼装的字样 ——
    /// 那些字样只允许出现在 `bg_output` 里。
    ///
    /// 钉住的变异：在本模块内联一份同样的拼装（哪怕逐字相同）⇒ 用例变红。
    #[test]
    fn the_output_assembly_lives_only_in_the_shared_module() {
        let source = include_str!("bg_tools.rs");
        let body = source
            .split_once("#[cfg(test)]")
            .expect("本模块必须有测试段")
            .0;
        for marker in ["lines omitted", "未送达（头尾裁剪）", "full output:"] {
            assert!(
                !body.contains(marker),
                "输出拼装只能有一处真源（`bg_output::assemble`）—— 本模块出现了 {marker:?}"
            );
        }
    }
}
