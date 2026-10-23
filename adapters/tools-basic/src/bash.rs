use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use ys_core::ToolResult;
use ys_protocol::BoundarySource;
use ys_tool::{Tool, ToolContext, ToolError, ToolSpec};

#[cfg(unix)]
use crate::proc_group;

/// 挂载了 `boundary` 时的中止轮询间隔 —— 命令中止的感知延迟上界。
const ABORT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 杀进程组之后等管道排空收束的上限（插话路径要取回已捕获的输出）。
const PIPE_SETTLE: Duration = Duration::from_secs(2);

/// 内层 deadline 与外层预算之间的**余量** —— 内层必须**严格早于**外层到期。
///
/// 外层预算到点时是把工具 future **drop** 掉（`ys-loop` 的 `tokio::time::timeout`），
/// 届时只有 `kill_on_drop` 生效（只杀直接子进程）⇒ 进程组不会被清理。
/// 故内层留出余量，让「超时」一律由**工具自己**发现 ⇒ 走「杀进程组」的那条分支。
const DEADLINE_MARGIN: Duration = Duration::from_secs(5);

/// 生效 deadline 的**下限** —— 外层预算过紧时不得算出 0 或负数。
const MIN_DEADLINE: Duration = Duration::from_secs(1);

/// 算出工具内的**生效 deadline**。
///
/// 两条上限同时生效：
/// - 模型传了 `timeout` ⇒ 上限是**模型值**；
/// - 外层预算（[`ToolContext::tool_budget`]）存在 ⇒ 上限是「**外层预算 − 余量**」。
///
/// 两者都有时取**更小**者；**模型没传 `timeout` 时仍然有 deadline**（= 外层预算 − 余量）。
/// 两者都没有 ⇒ `None`（既无外层预算也无模型值 ⇒ 沿用「跑到自然结束」的语义）。
///
/// 模型值**原样**参与取小（模型传 0 ⇒ 生效 deadline 就是 0）——
/// 那一档的超时文案写「0 seconds」（见 [`deadline_secs`]）。
///
/// **「无 deadline」档位的完成条件**：阶段 A 以「管道读到 EOF」为完成条件
/// ⇒ 若有孙进程持着写端，工具会**一直阻塞**到孙进程自己退出；
/// 唯一的打断是 boundary 的**两个信号**：中止（`Abort`）与**用户插话**
/// （`has_pending_steer()` —— 停手，让轮边界尽快把插话交给模型）。
/// 该档位的可达性：生产路径两路都必挂 `with_tool_budget` ⇒ **不可达**；
/// 但「挂 boundary、不挂 `tool_budget`、模型不传 `timeout`」的组合在
/// **直接调用工具**时可达（正常命令仍能正常返回，见
/// `test_no_deadline_tier_returns_a_normal_command`）。
fn effective_deadline(model: Option<Duration>, outer: Option<Duration>) -> Option<Duration> {
    match (model, outer) {
        (None, None) => None,
        (Some(model), None) => Some(model),
        (None, Some(outer)) => inner_from_outer(outer),
        (Some(model), Some(outer)) => match inner_from_outer(outer) {
            Some(inner) => Some(model.min(inner)),
            // 外层预算小到减不出内层 ⇒ 视作「没有外层预算」⇒ 只受模型值约束
            None => Some(model),
        },
    }
}

/// 由外层预算推出内层 deadline —— **严格早于**外层到期，且**为正**。
///
/// 外层预算减得掉余量时取「外层 − 余量」；减不掉时钳到**最小正秒数**，
/// 但仍须**严格早于**外层 ⇒ 外层本身已不足 1s 时取它的一半；
/// 外层不足 2ns（取半后为 0）⇒ 减不出「正的且严格早于外层」的值 ⇒ **无预算可用**
/// （`None` = 视作没有外层预算）。
fn inner_from_outer(outer: Duration) -> Option<Duration> {
    let inner = outer.saturating_sub(DEADLINE_MARGIN);
    if inner >= MIN_DEADLINE {
        return Some(inner);
    }
    if outer > MIN_DEADLINE {
        return Some(MIN_DEADLINE);
    }
    let half = outer / 2;
    if half.is_zero() { None } else { Some(half) }
}

/// 生效 deadline 的**秒数**（写进超时文案）—— 向上取整到整秒。
///
/// 文案写**生效值**：模型传 900、实际生效 295 ⇒ 文案说 295；
/// 生效值为 0（模型传 `timeout: 0`）⇒ 文案说 0 —— 与生效值**逐字一致**，
/// 不因「秒数为 0 看着不像超时」而擅自抬到 1。
fn deadline_secs(deadline: Duration) -> u64 {
    let nanos = deadline.as_nanos();
    u64::try_from(nanos.div_ceil(1_000_000_000)).unwrap_or(u64::MAX)
}

/// 等待阶段的结局 —— 「被中止」/「因插话提前收口」/「撞上生效 deadline」/「等待本身失败」。
///
/// `TimedOut` 携带**生效时长**：超时文案必须写生效值（模型传的值可能远大于它）。
enum WaitOutcome {
    /// 边界发出了中止信号。
    Aborted,
    /// 边界探到了**未消费的用户插话**（`has_pending_steer`）—— 停手，让 `BasicLoop`
    /// 尽快回到轮边界把插话交给模型。
    ///
    /// **不是失败**：已捕获的输出照常回给模型，只是命令未跑完。
    Steered,
    /// 撞上生效 deadline（携带生效时长）。
    TimedOut(Duration),
    /// 等待本身失败（`wait` 报错）。
    Failed(String),
}

/// 边界上的两个打断信号。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Interrupt {
    /// 整回合中止：以 `Cancelled` 收场。
    Abort,
    /// 用户插话待到轮边界消费：本命令提前收口，但回合继续。
    Steer,
}

/// 等到边界发出打断信号，并返回原因。
///
/// **先探 `Abort` 再探插话**：中止是更强的信号，两者同在时按中止处理（否则会为了
/// 「让模型读到插话」而把一个正在收场的回合拖下去）。
///
/// `None` = 没有边界控制 ⇒ **永不完成**（调用方的 `select!` 只会等另外两个分支）。
async fn wait_interrupt(boundary: Option<&dyn BoundarySource>) -> Interrupt {
    match boundary {
        Some(boundary) => loop {
            if boundary.is_aborted() {
                return Interrupt::Abort;
            }
            if boundary.has_pending_steer() {
                return Interrupt::Steer;
            }
            tokio::time::sleep(ABORT_POLL_INTERVAL).await;
        },
        None => std::future::pending::<Interrupt>().await,
    }
}

/// 等到生效 deadline 到点。
///
/// 携带生效时长，供超时文案写**生效值**。
/// `None` = 没有 deadline ⇒ **永不完成**。
async fn wait_deadline(deadline: Option<(Instant, Duration)>) {
    match deadline {
        Some((at, _)) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        None => std::future::pending::<()>().await,
    }
}

pub struct BashTool {
    workspace: PathBuf,
    /// 后台注册表 —— `Some` 时超时**不杀进程，改为过继**给后台并由完成通知送达。
    ///
    /// 装配期决定：交互式 TUI 挂载（有回合循环消费 `Boundary::Deferred`），
    /// `-p` / `--json` 不挂（单回合，结果无人接收）。
    backgrounds: Option<(
        std::sync::Arc<crate::background::BackgroundTasks>,
        crate::background::SessionSlot,
    )>,
}

impl BashTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            backgrounds: None,
        }
    }

    /// 挂载后台能力 —— 超时将**过继**给注册表，而不是杀掉进程组。
    ///
    /// 带上 `SessionSlot` 是因为注册表按会话隔离条目，而会话 id 只能运行时读
    /// （工具表是建表期产物 ⇒ 不能冻在工具里）。
    pub fn with_backgrounds(
        mut self,
        backgrounds: std::sync::Arc<crate::background::BackgroundTasks>,
        session: crate::background::SessionSlot,
    ) -> Self {
        self.backgrounds = Some((backgrounds, session));
        self
    }
}

/// 把子进程输出组装成工具结果：stdout + stderr 合并、超限尾截断、非零退码标错。
///
/// `note` 是这次调用的一条**事实**（当前唯一来源：[`redundant_root_cd`]），追加在
/// 正文之后、退码行之前。它**不影响** `is_error` —— 命令本身失败时，注记与
/// `Command exited with code …` 并存，且退码行仍是最后一行。
fn render_result(
    stdout: &[u8],
    stderr: &[u8],
    exit_code: Option<i32>,
    note: Option<&str>,
) -> ToolResult {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);

    let mut combined = String::new();
    if !stdout.is_empty() {
        combined.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }

    // 尾截断由 `ys_tool::truncate` 统一实现；「丢弃了多少行」的通知由本工具前插。
    // `total_lines - shown_lines` 即被丢弃的行数（= 原实现里的 `skipped`）。
    let truncated = ys_tool::tail(&combined, ys_tool::MAX_LINES, ys_tool::MAX_BYTES);
    let mut content = truncated.content;
    if truncated.shown_lines < truncated.total_lines {
        content.insert_str(
            0,
            &format!(
                "[Truncated {} lines.]\n\n",
                truncated.total_lines - truncated.shown_lines
            ),
        );
    }

    // 注记追加在**正文之后、退码行之前**（两条「关于本次执行」的元信息相邻）：
    // 截断通知紧贴它要说明的正文、不被隔开，且既有不变量「失败结果的最后一行是
    // 退码行」不发生位移。空输出（`cd <目录> && true`）时不前插空行。
    if let Some(note) = note {
        if !content.is_empty() {
            content.push_str("\n\n");
        }
        content.push_str(note);
    }

    // 检查退出码（被信号杀死时 code 为 None，不视作「非零退码」）
    if let Some(code) = exit_code {
        if code != 0 {
            return ToolResult {
                content: format!("{content}\n\nCommand exited with code {code}"),
                is_error: true,
            };
        }
    }

    ToolResult {
        content,
        is_error: false,
    }
}

/// 命令是否以「`cd` 到本工具 workspace 同一个目录」开头 —— 是即判为冗余。
///
/// 本工具每次调用都在 `self.workspace` 起 shell（见 `call` 里的 `current_dir`），
/// 故这条 `cd` 不产生任何位移，判定的唯一用途是**在结果里回一行事实**（模型无从
/// 核对「我在哪」，只能自己每轮钉一次位置）。
///
/// 决定这次调用**实际执行哪条命令**，以及要不要回一行事实注记。
///
/// 返回 `(实际执行的命令, 注记)`：
/// - 冗余前缀 **且** 逃生口关闭 ⇒ `(tail, "…was removed")` —— **真的剥离**；
/// - 冗余前缀 **但** 逃生口打开 ⇒ `(原命令, "…changed nothing")` —— 旧行为；
/// - 非冗余（含 `cd` 去别处）⇒ `(原命令, None)` —— **完全不动**。
///
/// **抽成纯函数**是为了可直接单测「剥离与否」这件事 —— 走 `call` 只能看到
/// 命令输出，而 `cd <ws> && X` 与 `X` 在当前目录相同时**输出完全一样** ⇒
/// 无法从输出判别是否剥离（注入假 `sh` 又要改进程 `PATH`，与并行测试冲突）。
fn plan_cd_strip<'a>(
    command: &'a str,
    workspace: &Path,
    keep_cd: bool,
) -> (&'a str, Option<String>) {
    match redundant_root_cd(command, workspace) {
        Some(tail) if !keep_cd => (
            tail,
            Some(format!(
                "[bash: this shell already started in {}; the leading cd was removed]",
                workspace.display()
            )),
        ),
        Some(_) => (
            command,
            Some(format!(
                "[bash: this shell already started in {}; the leading cd changed nothing]",
                workspace.display()
            )),
        ),
        None => (command, None),
    }
}

/// 只认**第一段**的 `cd <path> && 余下`：`<path>` 允许裸串 / 单引号 / 双引号；
/// `余下` 必须非空（`cd <目录> &&` 是语法错误，不是零位移）。两侧 `canonicalize`
/// 后比较，任一侧失败则退化为「相对路径按 `workspace` 拼接后字面比较」（此时
/// `cd .` 一类判不出来，只是少一行注记，不影响正确性）。
///
/// **不认**：`cd ~/…`、`cd $VAR`、`cd -`、`;` 分隔，以及 `cd <目录>` 之后不带命令的
/// 裸 `cd` —— 前几者要展开才知道指向何处，后者没有「下文命令」可省，报了没意义。
/// 指向 workspace **子目录**的 `cd` 是有意义的位移，一律不认。
fn redundant_root_cd<'a>(command: &'a str, workspace: &Path) -> Option<&'a str> {
    let rest = command.trim_start();
    let after = rest.strip_prefix("cd")?;
    // `cd` 之后必须是空白，否则是 `cdrom` / `cdparanoia` 这类命令词的开头
    if !after.starts_with(char::is_whitespace) {
        return None;
    }
    let (token, tail) = take_shell_token(after.trim_start())?;
    // 只认 `&&` 分隔：`;` 后的命令与这条 `cd` 不在同一「若非成功则不执行」链上。
    // 其后必须**真的有命令** —— `cd <目录> &&` 会被 `sh` 判为语法错误，不是零位移的 no-op。
    let rest = tail.trim_start().strip_prefix("&&")?;
    if rest.trim().is_empty() {
        return None;
    }

    let target = Path::new(token);
    // 空串（`cd "" && …`）在 `sh` 里是**失败**的切目录，不是 no-op；且
    // `workspace.join("")` 会退化成 `workspace/`，若不加这道守卫会被误判成冗余。
    if target.as_os_str().is_empty() {
        return None;
    }
    let resolved = if target.is_absolute() {
        target.to_path_buf()
    } else {
        workspace.join(target)
    };
    let same = match (resolved.canonicalize(), workspace.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        // 任一侧解析不到（目录不存在等）⇒ 退化为字面比较：宁可少说一行，不说错一句
        _ => resolved == workspace,
    };
    // 返回**余下的命令**（`&&` 之后）。调用方据此执行剥离后的命令。
    same.then(|| rest.trim_start())
}

/// 从头切出一个 shell 词：裸串读到空白或 shell 元字符（`&;|<>()`），
/// 引号串读到配对的同类引号。返回 `(词, 余下)`；引号未闭合时返回 `None`
/// （判不出就不判）。元字符也要断词，否则 `cd /a&&ls` 会把 `&&ls` 吃进路径。
fn take_shell_token(s: &str) -> Option<(&str, &str)> {
    let first = s.chars().next()?;
    if first == '\'' || first == '"' {
        let body = &s[first.len_utf8()..];
        let end = body.find(first)?;
        Some((&body[..end], &body[end + first.len_utf8()..]))
    } else {
        let end = s
            .find(|c: char| c.is_whitespace() || "&;|<>()".contains(c))
            .unwrap_or(s.len());
        (end > 0).then(|| (&s[..end], &s[end..]))
    }
}

/// 尽力终止整个进程组（含孙进程）—— 委托共享内核 [`proc_group::kill_group_async`]。
///
/// 内核的落点与「**必须先杀后收**」「pid 复用窗口」两条告诫写在
/// [`proc_group::kill_group`] 的文档里 —— 本 crate 只有那一份实现。
#[cfg(unix)]
pub(crate) async fn kill_process_group(pid: u32) {
    proc_group::kill_group_async(pid).await;
}

/// 收场：先杀进程组（含孙进程），再杀直接子进程兜底，最后收尸组长。
///
/// **顺序不可颠倒**：组长一旦被收尸，它的 pid 就可能被复用，
/// 此时 `kill -9 -{pid}` 可能打到无关的进程组（见 [`proc_group::kill_group`]）。
async fn terminate(child: &mut tokio::process::Child, pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        kill_process_group(pid).await;
    }
    #[cfg(not(unix))]
    let _ = pid;
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// 收场（杀进程组）之后取回已捕获的管道内容 —— **有界等待**。
///
/// 进程组被杀 ⇒ 写端关闭 ⇒ 两个排空任务必然收束，故通常**立即**返回。
/// 但「孙进程没随组而死」仍可能发生（杀组失败 / 已被过继走）—— 那种情况下
/// 不得把整轮拖住，到期即放弃并 `abort` 两个任务（已不需要它们的结果）。
async fn drain_after_kill(
    out_task: &mut tokio::task::JoinHandle<Vec<u8>>,
    err_task: &mut tokio::task::JoinHandle<Vec<u8>>,
) -> (Vec<u8>, Vec<u8>) {
    let settle = async {
        let (out, err) = tokio::join!(&mut *out_task, &mut *err_task);
        (out.unwrap_or_default(), err.unwrap_or_default())
    };
    match tokio::time::timeout(PIPE_SETTLE, settle).await {
        Ok(pair) => pair,
        Err(_) => {
            out_task.abort();
            err_task.abort();
            (Vec::new(), Vec::new())
        }
    }
}

/// 因**用户插话**提前收口的结果。
///
/// **不是错误**：命令确实跑了（只是没跑完），已捕获的输出原样交还，并在末尾说清
/// 「为什么停在这里」与「接下来会发生什么」—— 模型据此决定是重跑、接着跑还是先回应用户。
/// 插话本身仍排在边界队列里，由 `BasicLoop` 在**轮边界**消费，本函数**不碰**队列。
fn steered_result(stdout: &[u8], stderr: &[u8], note: Option<&str>) -> ToolResult {
    const STEER_NOTE: &str = "[bash: command stopped early because the user sent a message while \
         it was running. That message is queued and will enter your context as soon as this batch \
         of tool calls finishes — read it before deciding what to do next. Output captured before \
         the stop is above; anything after that point did not run.]";
    let combined = match note {
        Some(note) => format!("{note}\n{STEER_NOTE}"),
        None => STEER_NOTE.to_string(),
    };
    // 退出码传 `None`：是我们杀掉的，`code` 不承载语义（见 `render_result` 的既有约定）。
    render_result(stdout, stderr, None, Some(&combined))
}

#[async_trait::async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "bash",
            "Execute shell commands via `sh -c` (POSIX sh, not the user's interactive shell). \
             Supports timeout. Every call starts a fresh shell in the workspace root; relative \
             paths resolve against it, and `cd` does NOT carry over between calls.",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" },
                    "timeout": { "type": "number", "description": "Timeout in seconds" }
                },
                "required": ["command"]
            }),
        )
    }

    async fn call(&self, input: Value, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let command = input["command"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing 'command'".into()))?;
        let timeout_secs = input["timeout"].as_u64();

        // 工具内**始终**有 deadline：模型传的 `timeout` 与外层预算（`ctx.tool_budget`）
        // 取更小者；模型没传 `timeout` 时取「外层预算 − 余量」。
        // ⇒ 超时一律由**工具自己**发现 ⇒ 走「杀进程组」的那条分支，
        // 而非外层 drop future 时只触发 `kill_on_drop`。
        let deadline = effective_deadline(timeout_secs.map(Duration::from_secs), ctx.tool_budget);

        // 工作目录不成立 ⇒ 硬报错。放行到 `spawn` 只会得到一句
        // `Failed to spawn command`，与「命令本身有问题」无法区分。
        match tokio::fs::metadata(&self.workspace).await {
            Ok(meta) if !meta.is_dir() => {
                return Err(ToolError::Execution(format!(
                    "Working directory is not a directory: {}",
                    self.workspace.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::Execution(format!(
                    "Working directory does not exist: {}",
                    self.workspace.display()
                )));
            }
            // 存在且是目录，或「读不到但不是 NotFound」⇒ 交给原有 spawn 路径报
            _ => {}
        }

        // **冗余 `cd` 前缀：剥离后再执行**（C3）。
        //
        // 判定复用 [`redundant_root_cd`]（ADR-0039 已实现：canonicalize 比对、引号处理、
        // `cd <目录> &&` 语法错误排除）。旧行为是「照原样执行 + 回一行事实」，但实测
        // 该注记**没能降低前缀率**（真实会话 560 条 bash 里 554 条仍带前缀，99%）⇒
        // 改为**真的剥离**：工作目录本来就是 workspace，`cd <workspace>` 是零位移。
        //
        // **逃生口**：`YUSHAN_KEEP_CD=1` 保留旧行为（只注记、不剥离）—— 万一某条命令
        // 依赖「`cd` 出现在命令行里」本身（如脚本自省 `$0`/`$@`），可关掉。
        //
        // ⚠️ **守卫不可省**：真实会话里 **7 条**命令是 `cd <其它目录> && …`（如
        // `cd ~/.yushan/skills/x && ls`）—— 那是**有意义的位移**，`redundant_root_cd`
        // 的 canonicalize 比对会放过它们。这也修正了 ADR-0039 的样本结论
        //（「无一条是 cd 去别处」在本次样本不成立）。
        let keep_cd = std::env::var_os("YUSHAN_KEEP_CD").is_some_and(|v| !v.is_empty() && v != "0");
        let (effective_command, note) = plan_cd_strip(command, &self.workspace, keep_cd);

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(effective_command)
            .current_dir(&self.workspace)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // `kill_on_drop` 是**最后一道兜底**，不是主路径：
            // 主路径是工具自己发现 deadline / 中止 ⇒ 走 [`terminate`] 的「杀整个进程组」
            // ⇒ 孙进程随组而死。
            // 兜底只在**工具 future 被外层预算 drop**（`Child` 随之 drop）时生效，
            // 而它只杀**直接子进程** ⇒ 该路径下孙进程仍可能成为孤儿。
            .kill_on_drop(true);

        // Unix 上使用进程组
        #[cfg(unix)]
        {
            #[allow(unused_imports)]
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Execution(format!("Failed to spawn command: {e}")))?;

        let pid = child.id();

        // 管道由独立任务排空。**组长退出 ≠ 管道排空**：孙进程可能仍持着写端
        // （`sh -c 'sleep 900 &'` 这类）⇒ 排空必须**自己**受生效 deadline 约束，
        // 否则工具会越过自己的 deadline 一直阻塞（孙进程攥着写端不放）。
        let mut stdout_pipe = child.stdout.take().expect("stdout 已 piped");
        let mut stderr_pipe = child.stderr.take().expect("stderr 已 piped");
        let mut out_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stdout_pipe.read_to_end(&mut buf).await;
            buf
        });
        let mut err_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut buf).await;
            buf
        });

        // 生效 deadline 的**绝对时刻**与时长 —— 两个阶段共用同一份上界。
        let deadline_at = deadline.map(|d| (Instant::now() + d, d));
        let boundary = ctx.boundary;

        // ── 阶段 A：排空管道 ──
        // 完成条件是「管道读到 EOF」，**不是**「组长退出」——
        // 后者会让「组长已退、孙进程持管道」的形状越过 deadline 一直阻塞。
        //
        // **不变量：阶段 A 绝不调用 `try_wait` / `wait`。**
        // 「先杀后收」（见 [`terminate`]）成立的前提是「组长在阶段 A 期间仍未被收尸」：
        // 一旦在这里收尸，组长的 pid 就可能被复用 ⇒ 随后的 `kill -9 -{pid}`
        // 会打到无关的进程组（见 [`proc_group::kill_group`]）。
        // 该前提依赖 tokio 的 reaper 是**惰性**的 —— 只有被 poll 才会 `waitpid`
        // （tokio 1.53 的 `reap.rs` / `pidfd_reaper.rs` 两条路径皆如此）。
        // ⇒ 在阶段 A 加一次 `try_wait()` 轮询前，必须先重新评估这条不变量。
        let drained = {
            let drain = async {
                let (out, err) = tokio::join!(&mut out_task, &mut err_task);
                (out.unwrap_or_default(), err.unwrap_or_default())
            };
            tokio::select! {
                biased;
                result = drain => Ok(result),
                reason = wait_interrupt(boundary) => Err(match reason {
                    Interrupt::Abort => WaitOutcome::Aborted,
                    Interrupt::Steer => WaitOutcome::Steered,
                }),
                _ = wait_deadline(deadline_at) => Err(WaitOutcome::TimedOut(
                    deadline_at.map(|(_, d)| d).unwrap_or_default(),
                )),
            }
        };

        // ── 阶段 B：组长退出并收尸 ──
        // 收尸放在**最后**：组长一旦被收尸，它的 pid 就可能被复用，
        // 此时再 `kill -9 -{pid}` 可能打到无关的进程组（见 [`proc_group::kill_group`]）。
        let outcome = match drained {
            Err(outcome) => Err(outcome),
            Ok((stdout, stderr)) => {
                let wait = child.wait();
                tokio::pin!(wait);
                let exited = tokio::select! {
                    biased;
                    result = &mut wait => match result {
                        Ok(status) => Ok(status.code()),
                        Err(e) => Err(WaitOutcome::Failed(e.to_string())),
                    },
                    reason = wait_interrupt(boundary) => Err(match reason {
                        Interrupt::Abort => WaitOutcome::Aborted,
                        Interrupt::Steer => WaitOutcome::Steered,
                    }),
                    _ = wait_deadline(deadline_at) => Err(WaitOutcome::TimedOut(
                        deadline_at.map(|(_, d)| d).unwrap_or_default(),
                    )),
                };
                match exited {
                    Ok(exit_code) => Ok((stdout, stderr, exit_code)),
                    Err(outcome) => Err(outcome),
                }
            }
        };

        match outcome {
            Ok((stdout, stderr, exit_code)) => {
                Ok(render_result(&stdout, &stderr, exit_code, note.as_deref()))
            }
            // **超时转后台**：进程不杀，过继给注册表，结果由完成通知送达。
            //
            // 前提：已挂载后台能力（交互式 TUI）。`-p` / `--json` 不挂 ——
            // 那些模式没有回合循环消费 `Boundary::Deferred`，转后台等于丢结果，
            // 故它们走下面的原路径（杀进程组 + 明确报错）。
            Err(WaitOutcome::TimedOut(deadline)) if self.backgrounds.is_some() => {
                let (tasks, slot) = self.backgrounds.as_ref().expect("已断言 Some");
                let tasks = tasks.clone();
                let session = slot.get();
                // 过继：只登记 pid（tokio 的 `Child` 无法转 std，收尸交给 watcher）。
                let Some(pid) = pid else {
                    return Err(ToolError::Execution(
                        "command timed out but its pid is unknown — cannot hand it off".into(),
                    ));
                };
                let handle =
                    tasks.adopt_pid(pid, &session, command.to_string(), self.workspace.clone());

                // watcher：接管**已经在跑**的排空任务（它们已 `take()` 走管道 ⇒
                // 再也拿不到 `child.stdout`），等两者都结束 + 进程退出后推终态。
                // 顺序必须是「先排空再 wait」：孙进程持写端时 `read_to_end` 不返回，
                // 而 `wait` 也可能因此迟迟不返回 —— 与阶段 A/B 的既有纪律一致。
                tokio::spawn(async move {
                    let mut child = child;
                    let (out, err) = tokio::join!(out_task, err_task);
                    let tail_out = out.unwrap_or_default();
                    let tail_err = err.unwrap_or_default();
                    if !tail_out.is_empty() || !tail_err.is_empty() {
                        let mut tail = String::new();
                        tail.push_str(&String::from_utf8_lossy(&tail_out));
                        tail.push_str(&String::from_utf8_lossy(&tail_err));
                        tasks.append_output(handle, &tail);
                    }
                    let code = child.wait().await.ok().and_then(|s| s.code());
                    tasks.mark_finished(handle, code);
                });

                Ok(ToolResult {
                    content: format!(
                        "Command exceeded its {}s timeout and is now running in the background.\n\
                         handle: {}\n\
                         Its result will be delivered to you automatically when it finishes — \
                         do NOT poll for it. Keep working on your own part now.",
                        deadline_secs(deadline),
                        handle.label()
                    ),
                    is_error: false,
                })
            }
            // 因**用户插话**提前收口：与中止同样要杀进程组（否则孙进程攥着管道），
            // 但结局不同 —— 已捕获的输出照样回给模型，回合继续。
            Err(WaitOutcome::Steered) => {
                terminate(&mut child, pid).await;
                let (stdout, stderr) = drain_after_kill(&mut out_task, &mut err_task).await;
                Ok(steered_result(&stdout, &stderr, note.as_deref()))
            }
            Err(outcome) => {
                // 中止 / 超时（未挂后台能力）/ 等待失败：先杀进程组（含孙进程），再杀子进程兜底，最后收尸
                terminate(&mut child, pid).await;
                // 已丢弃输出，读取任务不必再等（孙进程可能仍持写端）
                out_task.abort();
                err_task.abort();
                Err(match outcome {
                    WaitOutcome::Aborted => ToolError::Execution("aborted by user".into()),
                    // 上面已单独处理
                    WaitOutcome::Steered => unreachable!("Steered 已在上一分支处理"),
                    WaitOutcome::TimedOut(deadline) => ToolError::Timeout(format!(
                        "Command timed out after {} seconds",
                        deadline_secs(deadline)
                    )),
                    WaitOutcome::Failed(msg) => {
                        ToolError::Execution(format!("Failed to wait for command: {msg}"))
                    }
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx() -> ToolContext<'static> {
        ToolContext::new(None, PathBuf::from("."), PathBuf::from("."))
    }

    /// **描述即契约，但只陈述事实**：`bash` 每次调用都在 `workspace` 起新 shell、
    /// 相对路径按该目录解析、`cd` 不跨调用 —— 这三条事实必须留在模型读得到的
    /// `description` 里（`ToolSpec` 随每轮请求发给模型，命中前缀缓存，零边际成本）。
    ///
    /// **刻意不写禁令**（不写「不要 cd」），也不给 `cd dir && cmd` 这类许可 ——
    /// 禁令里夹一个许可会让模型把「这一条也需要」判给自己。实测依据见
    /// `docs/arch/bash-cwd-contract/context.md`。
    ///
    /// 变异：① 描述改回一句 `Execute shell commands. Supports timeout.` ⇒ 前两条断言变红；
    /// ② 把旧文本的 `do not prefix … or chain \`cd dir && cmd\`` 加回来 ⇒ 第三条变红。
    #[test]
    fn spec_description_states_the_cwd_contract() {
        let tool = BashTool::new(PathBuf::from("."));
        let d = tool.spec().description;
        assert!(
            d.contains("workspace root"),
            "描述须说明每次调用的工作目录；description = {d}"
        );
        assert!(
            d.contains("`cd` does NOT") && d.contains("carry over"),
            "描述须说明 `cd` 不跨调用保留（否则模型仍会防御性加前缀）；description = {d}"
        );
        assert!(
            !d.contains("system prompt"),
            "描述不得依赖可能被 SYSTEM.md 短路掉的系统提示文本；description = {d}"
        );
        assert!(
            !d.contains("do not prefix") && !d.contains("or chain"),
            "描述只陈述事实，不写禁令与逃生口；description = {d}"
        );
    }

    /// 临时目录名含**进程内原子序号 + `process::id()`**（并行安全的既有样板，见 `read.rs`）。
    fn test_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "agent_tools_basic_bash_test_{id}_{}_{seq}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn test_bash_echo() {
        let tool = BashTool::new(PathBuf::from("."));
        let ctx = make_ctx();
        let input = json!({ "command": "echo hello" });
        let result = tool.call(input, ctx).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("hello"));
    }

    #[tokio::test]
    async fn test_bash_nonzero_exit() {
        let tool = BashTool::new(PathBuf::from("."));
        let ctx = make_ctx();
        let input = json!({ "command": "exit 42" });
        let result = tool.call(input, ctx).await.unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("exited with code 42"));
    }

    /// **超时转后台**：已挂后台能力时，超时**不杀进程**，而是过继给注册表并返回句柄。
    ///
    /// 事件路径：`bash(sleep 30, timeout 1)` ⇒ 1s 后返回 `Ok`（`is_error: false`）
    /// + 文案含「handle: bgN」「do NOT poll」；随后进程仍在跑，由 watcher 收尾。
    ///
    /// 变异：① 去掉那个 `if self.backgrounds.is_some()` 守卫 ⇒ 走回 `terminate`，
    /// 本用例的 `Ok` 断言变红；② 文案改成不报错就返回成功但不给句柄 ⇒ `handle:` 断言红。
    #[tokio::test]
    async fn test_bash_timeout_hands_off_to_background_when_mounted() {
        let dir = std::env::temp_dir().join(format!(
            "ys-bash-adopt-{}-{}",
            std::process::id(),
            ADOPT_TEST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tasks = std::sync::Arc::new(crate::background::BackgroundTasks::new(
            dir.clone(),
            crate::background::BgLimits::default(),
        ));
        let slot = crate::background::SessionSlot::new("adopt-test");
        let tool = BashTool::new(dir.clone()).with_backgrounds(tasks.clone(), slot);

        let input = json!({ "command": "sleep 30", "timeout": 1 });
        let result = tokio::time::timeout(Duration::from_secs(10), tool.call(input, make_ctx()))
            .await
            .expect("超时应由工具自己发现")
            .expect("已挂后台 ⇒ 超时应返回 Ok（转后台），不是 Err");

        assert!(!result.is_error, "转后台不是错误");
        assert!(
            result.content.contains("handle: bg"),
            "必须告诉模型句柄；实际 = {}",
            result.content
        );
        assert!(
            result.content.contains("do NOT poll"),
            "必须明确禁止轮询；实际 = {}",
            result.content
        );

        // 进程没被杀：注册表里有一条仍在跑（或刚结束）的条目。
        assert!(
            !tasks.running_handles("adopt-test").is_empty(),
            "过继的条目应仍在注册表里"
        );

        // 收拾：杀掉那条，避免留下 30s 的 sleep。
        tasks.kill_all("adopt-test");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 超时**之后**写出的输出也必须落盘 —— watcher 要接管既有的排空任务。
    ///
    /// 变异：让 watcher 重新 `child.stdout.take()`（管道已被排空任务 `take()` 走 ⇒
    /// 恒 `None`）⇒ 尾部那段丢失，本用例的 `late` 断言变红。
    #[tokio::test]
    async fn test_handed_off_command_keeps_output_written_after_the_timeout() {
        let dir = std::env::temp_dir().join(format!(
            "ys-bash-adopt-tail-{}-{}",
            std::process::id(),
            ADOPT_TEST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tasks = std::sync::Arc::new(crate::background::BackgroundTasks::new(
            dir.clone(),
            crate::background::BgLimits::default(),
        ));
        let tool = BashTool::new(dir.clone()).with_backgrounds(
            tasks.clone(),
            crate::background::SessionSlot::new("tail-test"),
        );

        // 1s 超时；超时**后**才打印 `late` ⇒ 只有在 watcher 接管管道时才可能读到。
        let input = json!({
            "command": "sleep 2; echo late",
            "timeout": 1
        });
        let result = tokio::time::timeout(Duration::from_secs(10), tool.call(input, make_ctx()))
            .await
            .expect("超时应由工具自己发现")
            .expect("已挂后台 ⇒ 应转后台");
        assert!(!result.is_error);

        // 等 watcher 收尾（进程 2s 后才退出，多等一会儿）。
        let h0 = tasks
            .running_handles("tail-test")
            .first()
            .copied()
            .expect("过继后应有一条在跑条目");
        let path0 = tasks.output_path_of(h0).expect("条目应有输出路径");
        let mut text = String::new();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            tasks.sweep();
            let handles = tasks.running_handles("tail-test");
            let done = handles.iter().all(|h| {
                tasks
                    .snapshot(*h, "tail-test")
                    .is_some_and(|s| s.state != crate::background::BgState::Running)
            });
            if done {
                text = std::fs::read_to_string(&path0).unwrap_or_default();
                break;
            }
        }
        assert!(
            text.contains("late"),
            "超时之后写出的输出必须落盘（watcher 要接管既有排空任务）；实际 = {text:?}"
        );

        tasks.kill_all("tail-test");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 序列号 —— 临时目录名要含「进程内原子序号 + pid」（并行测试会撞名）。
    static ADOPT_TEST_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[tokio::test]
    async fn test_bash_timeout() {
        let tool = BashTool::new(PathBuf::from("."));
        let ctx = make_ctx();
        let input = json!({ "command": "sleep 30", "timeout": 1 });
        // 墙钟上界：超时必须由**工具自己**发现（工具内没有 deadline 时会挂到 sleep 自然结束）
        let result = tokio::time::timeout(Duration::from_secs(10), tool.call(input, ctx))
            .await
            .expect("超时应由工具自己发现（10s 内返回）");

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::Timeout(_)));
    }

    /// N3（不泄漏）：超时后**直接子进程必须已被杀死**，不得继续执行后续命令。
    ///
    /// 原理：超时由**工具自己**发现 ⇒ 走 [`terminate`] 的收场序列
    /// （先杀整个进程组含孙进程，再杀直接子进程兜底，最后收尸组长）。
    /// `sh` 死于 1s，来不及在 2s 执行 `touch` ⇒ 故 3s 后 sentinel 必须不存在。
    ///
    /// ⚠️ 本用例只用「`sh` 亲自执行 `touch`」的形式 —— 孙进程是否随组而死，
    /// 由 `test_bash_timeout_kills_grandchild_without_boundary` 单独覆盖；
    /// `kill_on_drop`（外层 drop 工具 future 时的兜底）由
    /// `test_dropping_the_tool_future_kills_the_direct_child` 单独覆盖。
    ///
    /// 变异：**同时**去掉两处终止 —— `.kill_on_drop(true)` 与 `terminate` 里的
    /// 「杀进程组 + 杀直接子进程」⇒ `sh` 活过 2s 并 `touch` ⇒ sentinel 存在 ⇒ 红。
    /// （只去掉其中一处仍绿：两处各自都能单独杀掉 `sh` ——
    /// `terminate` 是工具自己发现超时时走的收场序列，`kill_on_drop` 是工具
    /// 返回、`Child` 被 drop 时的兜底。）
    #[tokio::test]
    async fn test_bash_timeout_does_not_leak_child_process() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let sentinel = dir.join("leaked.txt");

        let tool = BashTool::new(dir.clone());
        let ctx = make_ctx();
        let input = json!({
            "command": format!("sleep 2; touch {}", sentinel.display()),
            "timeout": 1
        });

        let started = Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(10), tool.call(input, ctx))
            .await
            .expect("call 应在 ~1s 内返回；超过 10s 视为挂起（死锁必须表现为失败）");
        let elapsed = started.elapsed();

        let err = result.expect_err("超时必须返回 Err");
        assert!(
            matches!(err, ToolError::Timeout(_)),
            "expected Timeout, got {err:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "超时应按 timeout 收敛（1s），实际耗时 {elapsed:?}"
        );

        // 等过命令里 `sleep 2` 的时点：若子进程没被杀死，此刻 sentinel 已被 touch 出来。
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !sentinel.exists(),
            "超时后子进程仍存活并执行了 touch（终止动作未生效）：{}",
            sentinel.display()
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 工具 future 被**外部 drop** ⇒ `kill_on_drop` 是唯一生效的终止手段。
    ///
    /// 场景：外层预算到点时把工具 future drop 掉（`ys-loop` 的 `tokio::time::timeout`），
    /// 工具来不及走「杀进程组」那条收场序列 ⇒ 只剩 `kill_on_drop` 杀**直接子进程**。
    /// 该路径**只覆盖直接子进程**（孙进程仍可能成孤儿）。
    ///
    /// 判据是**进程存在性**（按 pid 查 `ps`），不是错误文案。
    ///
    /// 变异：删掉 `.kill_on_drop(true)` → 直接子进程活过 5s → 断言变红。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_dropping_the_tool_future_kills_the_direct_child() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let pid_file = dir.join("leader.pid");
        let secs = unique_sleep_secs(&dir);
        let marker = format!("sleep {secs}");
        // 残留守卫：断言 panic 时也不留孤儿
        let _guard = LeftoverGuard::new(&pid_file, &marker);

        let tool = BashTool::new(dir.clone());
        let ctx = make_ctx();
        // `exec sleep <secs>` 让组长**自己**变成那个 `sleep`（pid 不变）⇒ 没有孙进程，
        // 残留面只有这一个进程。
        let input = json!({
            "command": format!("echo $$ > '{}'; exec sleep {secs}", pid_file.display())
        });

        let handle = tokio::spawn(async move { tool.call(input, ctx).await });
        let pid = read_pid(&pid_file).await;
        assert_eq!(
            grandchild_alive(pid, &marker),
            Some(true),
            "组长必须已经起来（pid {pid}）"
        );

        // 模拟外层预算到点：drop 工具 future ⇒ `Child` 被 drop ⇒ `kill_on_drop` 生效
        handle.abort();
        let _ = handle.await;

        let gone = wait_until_gone(pid, &marker, Duration::from_secs(5)).await;
        assert!(
            gone,
            "工具 future 被 drop 后直接子进程必须已被 kill_on_drop 杀掉（pid {pid}）"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── D2：超时后孙进程必须一起消失 ──────────────────────────────

    /// 读一份 `ps` 快照 —— 失败诊断用（进程已消失时返回空串）。
    #[cfg(unix)]
    fn ps_snapshot(pid: u32) -> String {
        std::process::Command::new("ps")
            .arg("-p")
            .arg(pid.to_string())
            .arg("-o")
            .arg("stat=,command=")
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .unwrap_or_else(|e| format!("<ps 调用失败: {e}>"))
    }

    /// 失败路径的清理：把**残留**的孙进程按 pid 杀掉。
    ///
    /// 只在孙进程**仍然活着**（按唯一的 `sleep <secs>` 标记确认身份）时才动手
    /// ⇒ 不会误伤 pid 已被复用的无关进程。
    ///
    /// 只杀孙进程本身：它的进程组 leader（`sh`）由工具自己的清理 / `kill_on_drop` 负责。
    #[cfg(unix)]
    async fn kill_leftover_grandchild(pid_file: &Path, marker: &str) {
        let Ok(text) = tokio::fs::read_to_string(pid_file).await else {
            return;
        };
        let Ok(pid) = text.trim().parse::<u32>() else {
            return;
        };
        kill_grandchild(pid, marker);
    }

    /// 拼一条「后台跑 `sleep` 并把它的 pid 写进文件」的命令 —— **按 pid** 认领孙进程，
    /// 不靠进程名猜。
    ///
    /// `sleep` 的秒数取自测试目录名里的序号 ⇒ 本进程内唯一 ⇒ 判「还在不在」时
    /// 不会撞上别的同名 `sleep`（pid 复用也不怕）。
    fn grandchild_command(pid_file: &Path, secs: u64) -> String {
        format!("sleep {secs} & echo $! > '{}'; wait", pid_file.display())
    }

    /// 拼一条「**组长先退出**」的命令 —— 与 [`grandchild_command`] 只差一个 `wait`。
    ///
    /// 组长 `sh` 执行完 `echo $!` 就退出（`&` 之后不再等待）⇒
    /// **孙进程独自持有 stdout 管道写端** ⇒ 「组长已退出」与「管道已排空」是两件事。
    fn leader_exits_first_command(pid_file: &Path, secs: u64) -> String {
        format!("sleep {secs} & echo $! > '{}'", pid_file.display())
    }

    /// 工具调用的**墙钟上界** —— 工具内若没有 deadline，命令会挂到 `sleep` 自然结束
    /// ⇒ 没有这个上界，测试会**永久挂起**而不是快速失败。
    const TOOL_CALL_GUARD: Duration = Duration::from_secs(10);

    /// 残留孙进程的清理守卫 —— `Drop` 时按 pid 杀掉**残留**的孙进程。
    ///
    /// 让「断言 panic」这条路径也不留孤儿（`sleep 900000+` ≈ 10.4 天）。
    /// 只杀**按标记确认过身份**的进程 ⇒ 不会误伤 pid 已被复用的无关进程。
    struct LeftoverGuard {
        pid_file: PathBuf,
        marker: String,
    }

    impl LeftoverGuard {
        fn new(pid_file: &Path, marker: &str) -> Self {
            Self {
                pid_file: pid_file.to_path_buf(),
                marker: marker.to_string(),
            }
        }
    }

    impl Drop for LeftoverGuard {
        fn drop(&mut self) {
            // panic 展开时不能再 await ⇒ 同步清理
            let Ok(text) = std::fs::read_to_string(&self.pid_file) else {
                return;
            };
            let Ok(pid) = text.trim().parse::<u32>() else {
                return;
            };
            kill_grandchild(pid, &self.marker);
        }
    }

    /// 从测试目录名里取出序号 ⇒ 本次运行内唯一的 `sleep` 秒数。
    fn unique_sleep_secs(dir: &Path) -> u64 {
        let seq = dir
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.rsplit('_').next())
            .and_then(|seq| seq.parse::<u64>().ok())
            .unwrap_or(0);
        900_000 + seq
    }

    /// 该 pid 是否仍是**活着的孙进程**：进程存在、非僵尸、且命令就是那个 `sleep`。
    ///
    /// 用 `ps` 而不是 `kill -0`：僵尸进程仍然「存在」，但已不再运行。
    ///
    /// `None` = **`ps` 调用失败** ⇒ **无法判定**。调用方不得把它当成「进程已死」——
    /// 那会让「孙进程还在跑」的断言**假绿**。
    fn grandchild_alive(pid: u32, marker: &str) -> Option<bool> {
        let out = std::process::Command::new("ps")
            .arg("-p")
            .arg(pid.to_string())
            .arg("-o")
            .arg("stat=,command=")
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let Some(line) = text.lines().find(|line| !line.trim().is_empty()) else {
            return Some(false);
        };
        let (stat, command) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        if stat.contains('Z') {
            return Some(false);
        }
        Some(command.contains(marker))
    }

    /// 查一个 pid 的 `ps` 状态。
    ///
    /// - `None`：**`ps` 调用失败** ⇒ 无法判定（不得当成「进程已消失」）。
    /// - `Some(None)`：`ps` 跑通但查无此进程 ⇒ **已被收尸**（pid 已释放）。
    /// - `Some(Some(stat))`：状态字符串（`Z` = 僵尸 = 已退出、**未收尸**）。
    fn ps_stat(pid: u32) -> Option<Option<String>> {
        let out = std::process::Command::new("ps")
            .arg("-p")
            .arg(pid.to_string())
            .arg("-o")
            .arg("stat=")
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        Some(
            text.lines()
                .find(|line| !line.trim().is_empty())
                .map(|line| line.trim().to_string()),
        )
    }

    /// 按 pid 杀掉**残留**的孙进程（同步内核 —— `Drop` 守卫与异步清理共用）。
    ///
    /// 只在孙进程**仍然活着**（按唯一的 `sleep <secs>` 标记确认身份）时才动手
    /// ⇒ 不会误伤 pid 已被复用的无关进程。`ps` 调用失败 ⇒ 无法确认身份 ⇒ 不动手。
    fn kill_grandchild(pid: u32, marker: &str) {
        if grandchild_alive(pid, marker) != Some(true) {
            return;
        }
        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status();
    }

    /// 读 pid 文件（还没写好 ⇒ `None`）—— 观测循环用，不 panic。
    async fn read_pid_if_present(path: &Path) -> Option<u32> {
        let text = tokio::fs::read_to_string(path).await.ok()?;
        text.trim().parse::<u32>().ok()
    }

    /// 等孙进程 pid 文件出现并读出 pid（命令一起来就写，通常在工具返回前就已在）。
    async fn read_pid(path: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(text) = tokio::fs::read_to_string(path).await
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                return pid;
            }
            assert!(
                Instant::now() < deadline,
                "孙进程 pid 文件始终没出现：{}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// 等到该 pid 不再是活着的孙进程（或预算耗尽）。
    ///
    /// `ps` **无法判定**时返回 `false` —— 「判不出」不得当成「已消失」。
    async fn wait_until_gone(pid: u32, marker: &str, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        loop {
            match grandchild_alive(pid, marker) {
                Some(false) => return true,
                Some(true) => {}
                None => return false,
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// D2 · 无 boundary 路径：超时后**孙进程**必须不存在。
    ///
    /// 判据是**进程存在性**（按 pid 查 `ps`），不是错误文案 ——
    /// 错误文案在修与不修时一模一样。
    ///
    /// 变异：删掉无 boundary 分支的进程组杀 ⇒ `sleep` 活下来 ⇒ 断言变红。
    #[tokio::test]
    #[cfg(unix)]
    async fn test_bash_timeout_kills_grandchild_without_boundary() {
        assert_grandchild_killed(make_ctx()).await;
    }

    /// D2 · 有 boundary 路径：同样按进程存在性断言 —— 两路走同一份内核，行为必须一致。
    ///
    /// 变异：把有 boundary 分支的进程组杀去掉 ⇒ `sleep` 活下来 ⇒ 断言变红。
    #[tokio::test]
    #[cfg(unix)]
    async fn test_bash_timeout_kills_grandchild_with_boundary() {
        let boundary = ys_protocol::QueueBoundarySource::new();
        assert_grandchild_killed(ctx_with(&boundary)).await;
    }

    /// D2 断言的公共体：跑一条「后台 `sleep` + `wait`」的命令 ⇒ 断言**孙进程**消失。
    ///
    /// **三处等待都有明确上界**（工具调用 ≤ [`TOOL_CALL_GUARD`]、pid 文件出现 ≤ 3s、
    /// 孙进程消失 ≤ 3s）⇒ 注入 bug 时是**快速失败**（红），不会把测试/CI 挂死。
    ///
    /// 孙进程的 `sleep` 秒数是 900000+ 序号量级（**正常路径下绝不会被真的等到** ——
    /// 工具在 1s 处超时并杀掉进程组，孙进程随即消失）；这个量级只保证
    /// 「`sleep` 命令行在本进程内唯一」，判存活时不撞上别的同名 `sleep`。
    #[cfg(unix)]
    async fn assert_grandchild_killed(ctx: ToolContext<'_>) {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let pid_file = dir.join("grandchild.pid");
        let secs = unique_sleep_secs(&dir);
        let marker = format!("sleep {secs}");
        // 断言 panic 也不留孤儿
        let _guard = LeftoverGuard::new(&pid_file, &marker);

        let tool = BashTool::new(dir.clone());
        let input = json!({
            "command": grandchild_command(&pid_file, secs),
            "timeout": 1
        });

        let result = match tokio::time::timeout(TOOL_CALL_GUARD, tool.call(input, ctx)).await {
            Ok(result) => result,
            Err(_) => {
                // 工具没按时返回 ⇒ 命令挂到了 `sleep` 的自然结束 ⇒ 先清残留再失败
                kill_leftover_grandchild(&pid_file, &marker).await;
                panic!(
                    "工具调用未在 {TOOL_CALL_GUARD:?} 内返回 —— 工具内没有 deadline \
                     （命令会挂到 sleep 自然结束）"
                );
            }
        };
        let err = result.expect_err("超时必须返回 Err");
        assert!(
            matches!(err, ToolError::Timeout(_)),
            "expected Timeout, got {err:?}"
        );

        let pid = read_pid(&pid_file).await;
        let gone = wait_until_gone(pid, &marker, Duration::from_secs(3)).await;
        // 诊断快照在清理**之前**取 —— 清掉之后就看不到残留进程了
        let snapshot = ps_snapshot(pid);
        if !gone {
            kill_leftover_grandchild(&pid_file, &marker).await;
        }
        assert!(
            gone,
            "超时后孙进程仍在运行（{marker}，pid={pid}）—— 进程组没被杀；ps: {snapshot}"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── 组长先退出：管道排空必须同样受生效 deadline 约束 ──────────────

    /// 「组长先退出 + 孙进程持管道」的公共断言体。
    ///
    /// 命令形状：`sleep <secs> & echo $! > <pid_file>`（**不带 `wait`**）——
    /// 组长 `sh` 立刻退出，孙进程独自持有 stdout 管道写端。
    ///
    /// 判据：① 工具在**生效 deadline** 内返回 `Timeout`；
    /// ② **孙进程按 pid 查 `ps` 不存在**（进程组被杀）。
    ///
    /// 三处等待都有明确上界（工具调用 ≤ [`TOOL_CALL_GUARD`]、pid 文件出现 ≤ 3s、
    /// 孙进程消失 ≤ 3s）⇒ 注入 bug 时是**快速失败**（红），不会把测试挂死。
    #[cfg(unix)]
    async fn assert_leader_exited_grandchild_killed(
        ctx: ToolContext<'_>,
        timeout_secs: Option<u64>,
    ) {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let pid_file = dir.join("grandchild.pid");
        let secs = unique_sleep_secs(&dir);
        let marker = format!("sleep {secs}");
        // 断言 panic 也不留孤儿
        let _guard = LeftoverGuard::new(&pid_file, &marker);

        let tool = BashTool::new(dir.clone());
        let mut input = json!({ "command": leader_exits_first_command(&pid_file, secs) });
        if let Some(timeout_secs) = timeout_secs {
            input["timeout"] = json!(timeout_secs);
        }

        let started = Instant::now();
        let result = match tokio::time::timeout(TOOL_CALL_GUARD, tool.call(input, ctx)).await {
            Ok(result) => result,
            Err(_) => {
                panic!(
                    "工具调用未在 {TOOL_CALL_GUARD:?} 内返回 —— 管道排空未受生效 deadline 约束 \
                     （孙进程攥着管道写端不放，工具越过自己的 deadline 一直阻塞）"
                );
            }
        };
        let elapsed = started.elapsed();

        let err = result.expect_err("超时必须返回 Err");
        assert!(
            matches!(err, ToolError::Timeout(_)),
            "expected Timeout, got {err:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "超时应按生效 deadline 收敛（~1s），实际耗时 {elapsed:?}"
        );

        let pid = read_pid(&pid_file).await;
        let gone = wait_until_gone(pid, &marker, Duration::from_secs(3)).await;
        // 诊断快照在清理**之前**取 —— 清掉之后就看不到残留进程了
        let snapshot = ps_snapshot(pid);
        assert!(
            gone,
            "组长先退出后孙进程仍在运行（{marker}，pid={pid}）—— 进程组没被杀；ps: {snapshot}"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 边界路径 · **组长先退出**：孙进程独自持有 stdout 管道写端。
    ///
    /// 这条路径是**生产默认**（每回合都挂 boundary）⇒ 越过 deadline 阻塞会直接
    /// 卡住整轮。
    ///
    /// 变异：删掉「管道排空也受生效 deadline 约束」那一支 ⇒ 工具挂到 `sleep` 的
    /// 自然结束 ⇒ 墙钟守卫爆掉 ⇒ 断言变红。
    #[tokio::test]
    #[cfg(unix)]
    async fn test_boundary_path_kills_grandchild_when_leader_exits_first() {
        let boundary = ys_protocol::QueueBoundarySource::new();
        assert_leader_exited_grandchild_killed(ctx_with(&boundary), Some(1)).await;
    }

    /// 无边界路径 · **组长先退出**：与有 boundary 路径同款判据。
    ///
    /// 变异：把无 boundary 路径的管道排空改成「组长退出即算完」⇒ 孙进程留活 ⇒ 变红。
    #[tokio::test]
    #[cfg(unix)]
    async fn test_no_boundary_path_kills_grandchild_when_leader_exits_first() {
        assert_leader_exited_grandchild_killed(make_ctx(), Some(1)).await;
    }

    /// 切片 0a 验收 ②：模型**不传** `timeout` + 外层预算到点 ⇒ **进程组被杀**。
    ///
    /// 判据是**进程存在性**（按 pid 查 `ps`），不是错误文案。
    #[tokio::test]
    #[cfg(unix)]
    async fn test_outer_budget_kills_grandchild_without_model_timeout() {
        let ctx = ctx_with_budget(Duration::from_secs(6));
        assert_leader_exited_grandchild_killed(ctx, None).await;
    }

    /// 「先杀后收」的顺序判据。
    ///
    /// 组长一旦被收尸，它的 pid 就可能被复用 ⇒ 此时再 `kill -9 -{pid}` 可能打到
    /// 无关的进程组（见 `proc_group::kill_group`）⇒ 超时分支必须**先杀组、再收尸**。
    ///
    /// 观测（工具运行期间持续轮询组长的 `ps` 状态）：
    /// ① 组长已退出但仍是**僵尸**（状态含 `Z`）⇒ 收尸发生在杀组**之后**；
    /// ② **孙进程还活着**时组长不得被回收 —— 组长消失即说明顺序是「先收后杀」。
    #[tokio::test]
    #[cfg(unix)]
    async fn test_group_is_killed_before_the_leader_is_reaped() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let leader_file = dir.join("leader.pid");
        let pid_file = dir.join("grandchild.pid");
        let secs = unique_sleep_secs(&dir);
        let marker = format!("sleep {secs}");
        let _guard = LeftoverGuard::new(&pid_file, &marker);

        // 组长写下自己的 pid 后立刻退出（**不带 `wait`**）⇒ 孙进程独自持有管道写端。
        let command = format!(
            "echo $$ > '{}'; sleep {secs} & echo $! > '{}'",
            leader_file.display(),
            pid_file.display()
        );
        let tool = BashTool::new(dir.clone());
        let input = json!({ "command": command, "timeout": 1 });
        let ctx = make_ctx();

        let handle = tokio::spawn(async move { tool.call(input, ctx).await });

        let mut saw_zombie = false;
        let mut reaped_first: Option<String> = None;
        // 观测窗口有上界 —— 工具挂住时观测循环也必须收场（不得把测试挂死）
        let watch_until = Instant::now() + TOOL_CALL_GUARD;
        while !handle.is_finished() && Instant::now() < watch_until {
            if let Some(leader_pid) = read_pid_if_present(&leader_file).await {
                match ps_stat(leader_pid) {
                    // 组长已退出、**未收尸**（僵尸）⇒ 收尸发生在杀组之后
                    Some(Some(stat)) if stat.contains('Z') => saw_zombie = true,
                    // 组长查无此进程 ⇒ 已被收尸
                    Some(None) => {
                        if let Some(gc_pid) = read_pid_if_present(&pid_file).await {
                            // 给 SIGKILL 的落地留一点宽限，再判孙进程是否还活着
                            let grace = Instant::now() + Duration::from_millis(300);
                            while grandchild_alive(gc_pid, &marker) == Some(true)
                                && Instant::now() < grace
                            {
                                tokio::time::sleep(Duration::from_millis(20)).await;
                            }
                            if grandchild_alive(gc_pid, &marker) == Some(true) {
                                reaped_first = Some(format!(
                                    "组长（pid={leader_pid}）已被收尸，而孙进程（pid={gc_pid}）仍在运行"
                                ));
                                break;
                            }
                        }
                    }
                    // `ps` 无法判定 / 组长还在跑 ⇒ 不作结论
                    None => {}
                    Some(Some(_)) => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let result = tokio::time::timeout(TOOL_CALL_GUARD, handle)
            .await
            .expect("工具调用必须在墙钟上界内返回 —— 管道排空未受生效 deadline 约束")
            .expect("工具调用 task 不应 panic");
        let err = result.expect_err("超时必须返回 Err");
        assert!(
            matches!(err, ToolError::Timeout(_)),
            "expected Timeout, got {err:?}"
        );
        assert!(
            reaped_first.is_none(),
            "顺序错：{} ⇒ 是「先收后杀」（pid 复用窗口）",
            reaped_first.unwrap_or_default()
        );
        assert!(
            saw_zombie,
            "组长退出后应处于僵尸态（Z）—— 它**尚未被收尸**；\
             若实现先收后杀，组长会被立即回收，观察不到 Z"
        );

        let pid = read_pid(&pid_file).await;
        let gone = wait_until_gone(pid, &marker, Duration::from_secs(3)).await;
        let snapshot = ps_snapshot(pid);
        assert!(
            gone,
            "超时后孙进程仍在运行（{marker}，pid={pid}）—— 进程组没被杀；ps: {snapshot}"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn test_bash_stderr() {
        let tool = BashTool::new(PathBuf::from("."));
        let ctx = make_ctx();
        let input = json!({ "command": "echo err >&2" });
        let result = tool.call(input, ctx).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("err"));
    }

    // ── 边界控制：`ToolContext.boundary` 报 Abort 时中止命令 ──────────────

    /// 挂载了 `QueueBoundarySource` 的 ctx 构造辅助。
    fn ctx_with(boundary: &ys_protocol::QueueBoundarySource) -> ToolContext<'_> {
        ToolContext::new(Some(boundary), PathBuf::from("."), PathBuf::from("."))
    }

    /// 没有任何边界信号时，挂载 `boundary` 不得改变既有行为。
    #[tokio::test]
    async fn test_bash_boundary_present_without_abort_runs_normally() {
        let boundary = ys_protocol::QueueBoundarySource::new();
        let tool = BashTool::new(PathBuf::from("."));
        let input = json!({ "command": "echo ok" });

        let result = tool.call(input, ctx_with(&boundary)).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("ok"));
    }

    /// 命令启动前已置 Abort → 立刻收场，不跑满命令。
    #[tokio::test]
    async fn test_bash_aborted_before_start_returns_immediately() {
        use ys_protocol::Boundary;

        let boundary = ys_protocol::QueueBoundarySource::new();
        boundary.push(Boundary::Abort);
        assert!(boundary.is_aborted());

        let tool = BashTool::new(PathBuf::from("."));
        let input = json!({ "command": "sleep 10" });

        let started = Instant::now();
        let result = tool.call(input, ctx_with(&boundary)).await;
        let elapsed = started.elapsed();

        let err = result.expect_err("Abort 后不应成功返回");
        assert!(
            matches!(err, ToolError::Execution(ref m) if m.contains("abort")),
            "expected abort Execution error, got {err:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "Abort 应立即中止，实际耗时 {elapsed:?}"
        );
    }

    /// 命令运行**期间**收到 Abort → 进程被终止，`call` 很快返回（不等 10 秒）。
    #[tokio::test]
    async fn test_bash_abort_mid_command_kills_it() {
        use std::sync::Arc;
        use ys_protocol::{Boundary, QueueBoundarySource};

        let boundary = Arc::new(QueueBoundarySource::new());
        let producer = Arc::clone(&boundary);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            producer.push(Boundary::Abort);
        });

        let tool = BashTool::new(PathBuf::from("."));
        let input = json!({ "command": "sleep 10" });

        let started = Instant::now();
        let result = tool.call(input, ctx_with(&boundary)).await;
        let elapsed = started.elapsed();

        let err = result.expect_err("运行中 Abort 应终止命令");
        assert!(
            matches!(err, ToolError::Execution(ref m) if m.contains("abort")),
            "expected abort Execution error, got {err:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "不应等到命令自然结束（10s），实际耗时 {elapsed:?}"
        );
    }

    /// 挂载 `boundary` 时，既有的 `timeout` 语义仍生效（未投 Abort 也必须在到期时中止）。
    #[tokio::test]
    async fn test_bash_timeout_still_applies_with_boundary() {
        let boundary = ys_protocol::QueueBoundarySource::new();
        let tool = BashTool::new(PathBuf::from("."));
        let input = json!({ "command": "sleep 10", "timeout": 1 });

        let started = Instant::now();
        // 墙钟上界：命令的 `sleep 10` 远长于此 ⇒ 工具内没有 deadline 时会在这里卡住
        let result = tokio::time::timeout(
            Duration::from_secs(8),
            tool.call(input, ctx_with(&boundary)),
        )
        .await
        .expect("超时应由工具自己发现（8s 内返回）");
        let elapsed = started.elapsed();

        let err = result.expect_err("超时应中止");
        assert!(
            matches!(err, ToolError::Timeout(_)),
            "expected Timeout, got {err:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "超时应按 timeout 收敛（1s），实际耗时 {elapsed:?}"
        );
    }

    /// 命令运行**期间**用户插话 → 命令提前收口（杀进程组），但**不是错误**：
    /// 已捕获的输出照常回给模型，结果里说清「为什么停」与「插话马上到」。
    ///
    /// 四条断言各钉一条不变量：
    /// ① 墙钟远小于命令时长（探针生效，不再傻等 30s）；
    /// ② 结果是 `Ok` 且带插话中止说明（不得伪装成「跑完了」或报错）；
    /// ③ 孙进程按 pid 不存在（进程组被杀）；
    /// ④ 插话**仍在队列里**（工具不得偷吃用户的插话 —— 它只该在轮边界被消费）。
    ///
    /// 变异：删掉 `wait_interrupt` 的 `has_pending_steer` 分支 ⇒ 跑到 `sleep` 结束
    /// ⇒ ① 爆守卫、④ 虽然仍绿但耗时断言变红。
    #[tokio::test]
    #[cfg(unix)]
    async fn test_bash_steer_mid_command_stops_early_without_consuming_steer() {
        use ys_protocol::Boundary;

        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let pid_file = dir.join("grandchild.pid");
        let secs = unique_sleep_secs(&dir);
        let marker = format!("sleep {secs}");
        let _guard = LeftoverGuard::new(&pid_file, &marker);

        let boundary = std::sync::Arc::new(ys_protocol::QueueBoundarySource::new());
        let steer = Boundary::Steer(ys_core::Message {
            role: ys_core::Role::User,
            content: vec![ys_core::ContentBlock::Text {
                text: "停一下，先看别的".into(),
            }],
        });
        // 命令**跑起来之后**才插话（真实时序：插话总是在工具已在跑时到达）
        let producer = std::sync::Arc::clone(&boundary);
        let steer_for_task = steer.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            producer.push(steer_for_task);
        });
        assert!(!boundary.has_pending_steer(), "插话尚未到达");

        let tool = BashTool::new(dir.clone());
        // 先把输出吐出来（验证「已捕获的 stdout 照常返回」），再挂到 `wait` 上。
        let input = json!({
            "command": format!(
                "echo started; {} ; wait",
                grandchild_command(&pid_file, secs)
            )
        });

        let started = Instant::now();
        let result = tokio::time::timeout(TOOL_CALL_GUARD, tool.call(input, ctx_with(&boundary)))
            .await
            .expect("插话应让工具提前收口（不得跑到 sleep 自然结束）");
        let elapsed = started.elapsed();

        let result = result.expect("插话提前收口不是错误（命令确实跑了，只是没跑完）");
        assert!(
            elapsed < Duration::from_secs(5),
            "插话应立即收口（远小于 30s 级命令时长），实际耗时 {elapsed:?}"
        );
        assert!(
            result.content.contains("stopped early"),
            "结果须说清是插话导致的中止：{}",
            result.content
        );
        assert!(
            result.content.contains("started"),
            "已捕获的 stdout 不得丢：{}",
            result.content
        );
        assert!(!result.is_error, "插话中止不是错误");

        // ③ 进程组被杀（按 pid 查进程存在性，不靠文案）
        let pid = read_pid(&pid_file).await;
        let gone = wait_until_gone(pid, &marker, Duration::from_secs(3)).await;
        let snapshot = ps_snapshot(pid);
        if !gone {
            kill_leftover_grandchild(&pid_file, &marker).await;
        }
        assert!(
            gone,
            "插话收口后孙进程仍在运行（{marker}，pid={pid}）—— 进程组没被杀；ps: {snapshot}"
        );

        // ④ 插话**未被消费**：仍排着，等轮边界
        assert_eq!(
            boundary.len(),
            1,
            "工具不得消费用户的插话（只能由轮边界 take）"
        );
        assert!(boundary.has_pending_steer());
        assert_eq!(boundary.take(), Some(steer), "插话原样留在队列里");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 未超限：`truncate::tail` 原样返回（含尾随换行），无需通知。
    #[test]
    fn test_tail_truncate_short() {
        let input = "line1\nline2\nline3\n";
        let result = ys_tool::tail(input, ys_tool::MAX_LINES, ys_tool::MAX_BYTES);
        assert_eq!(result.content, input);
        assert_eq!(result.shown_lines, result.total_lines);
    }

    /// 超限：保留末尾（含最后一行），且确有行被丢弃 —— 后者等价于「调用方会拼出
    /// `[Truncated N lines.]` 通知」（文案由调用方拼，不在本模块）。
    #[test]
    fn test_tail_truncate_long() {
        let lines: Vec<String> = (1..=3000).map(|i| format!("line{i}")).collect();
        let input = lines.join("\n");
        let result = ys_tool::tail(&input, ys_tool::MAX_LINES, ys_tool::MAX_BYTES);
        assert!(result.shown_lines < result.total_lines);
        assert!(result.content.contains("line3000"));
    }

    /// 走**完整 `call` 路径**，钉住 `[Truncated N lines.]` 的字面文案与**具体 N**。
    ///
    /// 关键构造：输入必须让**字节闸**（51200）而非行数闸（2000）成为约束 ——
    /// 3000 行 × 40 字节 = 123000 字节，行数（3000）并未超过行数上限（2000），
    /// 故 `shown_lines` 完全由 `tail` 的字节规则（单行成本 `len() + 1`，含换行）决定：
    /// `51200 / 41 = 1248` 行可留 ⇒ 丢弃 `3000 - 1248 = 1752` 行。
    ///
    /// 变异：`crates/ys-tool/src/truncate.rs::tail` 里 `lines[i].len() + 1` 改成 `len()`
    /// → `51200 / 40 = 1280` 行可留 ⇒ N 变 1720 → 本测试的 `1752` 断言变红。
    #[tokio::test]
    async fn test_bash_truncation_notice_reports_exact_line_count() {
        let tool = BashTool::new(PathBuf::from("."));
        let ctx = make_ctx();
        // 输出 3000 行、每行恰好 40 个 'x'（纯 ASCII，字节数 = 字符数）。
        let input = json!({
            "command":
                "awk 'BEGIN { s=\"\"; for (i=0;i<40;i++) s=s \"x\"; for (i=0;i<3000;i++) print s }'"
        });

        let result = tool.call(input, ctx).await.unwrap();

        assert!(!result.is_error);
        assert!(
            result.content.starts_with("[Truncated 1752 lines.]\n\n"),
            "通知文案（含丢弃行数）必须精确；实际开头：{:?}",
            result.content.chars().take(80).collect::<String>()
        );
        // 通知 2 行（正文 + 空行）+ 保留的 1248 行。
        assert_eq!(
            result.content.lines().count(),
            2 + 1248,
            "通知后应恰好保留 1248 行正文"
        );
        // 尾截断的语义：保留的是**末尾**。
        assert!(
            result.content.lines().skip(2).all(|l| l == "x".repeat(40)),
            "保留正文应全为 40 字节行"
        );
    }

    // ==================== 冗余 `cd` 的事实回显（见 `redundant_root_cd`） ====================

    /// 判定只认「命令开头的 `cd` 落回 workspace 本身」= 零位移。纯函数用例，不起进程。
    ///
    /// 变异：① `(Ok(a), Ok(b)) => a == b` 改成恒真 ⇒ 下面「子目录不算冗余」那条变红；
    /// ② 丢掉 canonicalize、直接比 `resolved == workspace` ⇒ `cd <ws>/.` 那条变红。
    #[test]
    fn test_redundant_root_cd_accepts_only_zero_displacement_forms() {
        let dir = test_dir();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let ws = dir.canonicalize().unwrap();
        let abs = ws.display().to_string();
        let name = ws.file_name().unwrap().to_string_lossy().to_string();

        for cmd in [
            format!("cd {abs} && ls"),
            format!("cd {abs}&&ls"),
            format!("cd '{abs}' && ls"),
            format!("cd \"{abs}\" && ls"),
            format!("cd {abs}/. && ls"),
            format!("cd {abs}/../{name} && ls"),
            format!("  cd {abs} && ls"),
            format!("\ncd {abs} && ls"),
            "cd . && ls".to_string(),
            format!("cd {abs} && cd sub && ls"),
        ] {
            let tail = redundant_root_cd(&cmd, &ws);
            assert!(tail.is_some(), "应判为冗余：{cmd}");
            // 剥离后的余下命令**必须以 `ls` 结尾**（前缀确实被摘掉了）。
            assert!(
                tail.is_some_and(|t| t.ends_with("ls")),
                "剥离结果应是 `&& ` 之后的命令：{cmd} ⇒ {tail:?}"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 判定的**反例**：有真实位移的、不在开头的、静态判不出指向的，一律不判冗余。
    ///
    /// 变异：去掉 `after.starts_with(char::is_whitespace)` 词边界 ⇒ `cd<ws> …` 变红；
    /// 去掉 `&&` 之后的非空检查 ⇒ `cd <ws> &&` 变红。
    ///
    /// `ls && cd <ws> && make` 那条**不具变异敏感性**：实现是 `strip_prefix` 在
    /// `trim_start` 之后直接取首段，结构上就不可能匹配中段 —— 该用例只作意图记录。
    #[test]
    fn test_redundant_root_cd_rejects_everything_else() {
        let dir = test_dir();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let ws = dir.canonicalize().unwrap();
        let abs = ws.display().to_string();

        for cmd in [
            // 真实位移
            format!("cd {abs}/sub && ls"),
            "cd .. && ls".to_string(),
            "cd /tmp && ls".to_string(),
            // 不在开头：本模块只判开头的「开局钉位置」
            format!("ls && cd {abs} && make"),
            // 静态判不出指向何处，或语义不是路径
            "cd ~/x && ls".to_string(),
            "cd $ROOT && ls".to_string(),
            "cd - && ls".to_string(),
            "cd && ls".to_string(),
            // 词边界与空输入
            "cdx && ls".to_string(),
            // ↑ 这条其实是被「路径不等于 ws」挡下的；**下面这条**才是词边界守卫
            //   唯一的可观测点：没有守卫时 `cd<abs>` 会被切成 `cd` + `<abs>` ⇒ 误报。
            format!("cd{abs} && ls"),
            "cd".to_string(),
            "echo hi".to_string(),
            // 空串：`sh` 里 `cd ""` 是**失败**的切目录，不是 no-op
            "cd \"\" && ls".to_string(),
            // 没有下文命令可省 / 只认 `&&` 分隔
            format!("cd {abs}"),
            format!("cd {abs} ; ls"),
            format!("cd {abs} &&"),
        ] {
            assert!(
                redundant_root_cd(&cmd, &ws).is_none(),
                "不应判为冗余：{cmd}"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **C3 的核心**：冗余前缀**真的被剥离**，且注记文案随之改变。
    ///
    /// 走纯函数而非 `call`：`cd <ws> && X` 与 `X` 在当前目录相同时**输出完全一样**
    /// ⇒ 从 `call` 的输出无法判别是否剥离；注入假 `sh` 又得改进程 `PATH`
    /// （与同 crate 的并行用例冲突）。
    ///
    /// 变异：① `Some(tail) if !keep_cd` 分支返回 `command` ⇒ 第 ① 段变红；
    ///       ② 注记文案写回 "changed nothing" ⇒ 第 ① 段变红。
    #[test]
    fn plan_cd_strip_actually_strips_redundant_prefix() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let ws = dir.canonicalize().unwrap();
        let abs = ws.display().to_string();

        // ① 冗余 + 逃生口关闭 ⇒ 执行**剥离后**的命令，注记说 removed。
        let cmd = format!("cd {abs} && echo hi");
        let (effective, note) = plan_cd_strip(&cmd, &ws, false);
        assert_eq!(effective, "echo hi", "必须真的剥离");
        let note = note.expect("冗余时应回一行事实");
        assert!(note.contains("was removed"), "注记应说明已剥离：{note}");

        // ② 冗余 + 逃生口打开 ⇒ 原样执行，注记说 changed nothing（旧行为）。
        let (effective, note) = plan_cd_strip(&cmd, &ws, true);
        assert_eq!(effective, cmd, "逃生口打开时必须原样执行");
        let note = note.expect("仍应回一行事实");
        assert!(note.contains("changed nothing"), "{note}");

        // ③ 非冗余（`cd` 去**别处**）⇒ 完全不动、不注记。
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let other = format!("cd {abs}/sub && ls");
        let (effective, note) = plan_cd_strip(&other, &ws, false);
        assert_eq!(effective, other, "有位移的 cd 绝不剥离");
        assert!(note.is_none(), "非冗余不该注记");

        // ④ 无 `cd` 前缀 ⇒ 不动、不注记。
        let (effective, note) = plan_cd_strip("echo hi", &ws, false);
        assert_eq!(effective, "echo hi");
        assert!(note.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 走完整 `call` 路径：冗余 `cd` **被剥掉**（C3），结果里回一行**工具自撰的事实**，
    /// 其余调用一概没有；且命令自身的输出**逐字节不变**，`is_error` 语义不变。
    ///
    /// 变异：① 删掉 `render_result` 里的注记追加 ⇒ 第 ②③④ 段全红；
    /// ② 把追加改成 `insert_str(0, …)` ⇒ 「退码行仍在最后一行」变红；
    /// ③ 把 `effective_command` 换回 `command`（即不剥离）⇒ 仍绿（注记照旧）——
    ///    这一条**由 `plan_cd_strip_actually_strips_redundant_prefix` 单独钉住**。
    #[tokio::test]
    async fn test_redundant_cd_note_is_added_only_when_the_cd_has_no_displacement() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let ws = dir.canonicalize().unwrap();
        let abs = ws.display().to_string();
        let note =
            format!("[bash: this shell already started in {abs}; the leading cd was removed]");
        let tool = BashTool::new(ws.clone());

        // ① 无前缀 ⇒ 无注记，输出就是命令输出
        let plain = tool
            .call(json!({ "command": "echo hi" }), make_ctx())
            .await
            .unwrap();
        assert_eq!(plain.content, "hi\n");
        assert!(!plain.content.contains("[bash:"));

        // ② 有前缀 ⇒ 正文逐字节不变，注记追加其后
        let prefixed = tool
            .call(
                json!({ "command": format!("cd {abs} && echo hi") }),
                make_ctx(),
            )
            .await
            .unwrap();
        assert!(!prefixed.is_error);
        assert_eq!(prefixed.content, format!("hi\n\n\n{note}"));

        // ③ 命令失败：注记在，退码行仍是**最后一行**（既有不变量）
        let failed = tool
            .call(
                json!({ "command": format!("cd {abs} && exit 7") }),
                make_ctx(),
            )
            .await
            .unwrap();
        assert!(failed.is_error);
        assert!(failed.content.contains(&note));
        assert_eq!(
            failed.content.lines().last().unwrap(),
            "Command exited with code 7"
        );

        // ④ 空输出：注记独占一行，前面不空行
        let silent = tool
            .call(
                json!({ "command": format!("cd {abs} && true") }),
                make_ctx(),
            )
            .await
            .unwrap();
        assert_eq!(silent.content, note);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 注记与既有 `[Truncated N lines.]` 通知同时出现时的**相对顺序**：
    /// 截断通知在最前（必须紧贴它说明的正文），注记在最后。
    ///
    /// 变异：把注记改成前插 ⇒ 本用例的前两条断言变红。
    #[tokio::test]
    async fn test_redundant_cd_note_does_not_displace_the_truncation_notice() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let ws = dir.canonicalize().unwrap();
        let abs = ws.display().to_string();
        let tool = BashTool::new(ws.clone());

        let input = json!({
            "command": format!(
                "cd {abs} && awk 'BEGIN {{ s=\"\"; for (i=0;i<40;i++) s=s \"x\"; \
                 for (i=0;i<3000;i++) print s }}'"
            )
        });
        let result = tool.call(input, make_ctx()).await.unwrap();

        assert!(
            result.content.starts_with("[Truncated 1752 lines.]\n\n"),
            "截断通知仍须在最前；实际开头：{:?}",
            result.content.chars().take(60).collect::<String>()
        );
        assert!(
            result.content.ends_with(&format!(
                "[bash: this shell already started in {abs}; the leading cd was removed]"
            )),
            "注记应在最末"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 工作目录不成立 ⇒ **硬报错**，而不是让 `spawn` 报一句与「命令本身有问题」
    /// 无法区分的 `Failed to spawn command`（对齐 pi 的 `Working directory does not exist`）。
    ///
    /// 变异：删掉 `call` 里的前置校验 ⇒ 退回 spawn 失败路径 ⇒ 两条断言都变红。
    #[tokio::test]
    async fn test_missing_workspace_is_an_explicit_error() {
        let dir = test_dir();
        assert!(!dir.exists(), "本用例要求目录不存在");
        let tool = BashTool::new(dir.clone());

        let err = tool
            .call(json!({ "command": "echo hi" }), make_ctx())
            .await
            .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("Working directory does not exist"), "{msg}");
        assert!(msg.contains(&dir.display().to_string()), "{msg}");
    }

    /// 工作目录**是文件**（不是目录）⇒ 文案必须与「不存在」区分开，否则用户查错方向。
    #[tokio::test]
    async fn test_workspace_that_is_not_a_directory_is_an_explicit_error() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let file = dir.join("not-a-dir");
        tokio::fs::write(&file, "x").await.unwrap();
        let tool = BashTool::new(file.clone());

        let err = tool
            .call(json!({ "command": "echo hi" }), make_ctx())
            .await
            .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("is not a directory"), "{msg}");
        assert!(msg.contains(&file.display().to_string()), "{msg}");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── D1：工具内始终有 deadline，且超时文案写**生效值** ──────────────

    /// 挂上**外层工具预算**的上下文 —— 无 boundary。
    fn ctx_with_budget(budget: Duration) -> ToolContext<'static> {
        ToolContext::new(None, PathBuf::from("."), PathBuf::from(".")).with_tool_budget(budget)
    }

    /// 挂上**外层工具预算**且**挂 boundary** 的上下文。
    fn ctx_with_boundary_and_budget(
        boundary: &ys_protocol::QueueBoundarySource,
        budget: Duration,
    ) -> ToolContext<'_> {
        ToolContext::new(Some(boundary), PathBuf::from("."), PathBuf::from("."))
            .with_tool_budget(budget)
    }

    /// 模型传 `timeout: 900` + 外层 300s ⇒ 生效 deadline = **295s**（外层 − 余量），
    /// 取「模型值」的那支被否。
    ///
    /// 变异：把 `min` 改成直接取模型值 ⇒ 断言变红。
    #[test]
    fn test_effective_deadline_takes_the_smaller_of_model_value_and_outer_minus_margin() {
        let effective = effective_deadline(
            Some(Duration::from_secs(900)),
            Some(Duration::from_secs(300)),
        );
        assert_eq!(
            effective,
            Some(Duration::from_secs(295)),
            "生效 deadline 应为外层 300s 减余量 5s"
        );
        assert_eq!(deadline_secs(effective.unwrap()), 295);
    }

    /// 模型**不传** `timeout` + 外层 300s ⇒ **仍有 deadline**（295s），不是「无 deadline」。
    ///
    /// 变异：删掉「未传值时取外层 − 余量」那一支 ⇒ 断言变红。
    #[test]
    fn test_effective_deadline_without_model_timeout_is_outer_minus_margin() {
        let effective = effective_deadline(None, Some(Duration::from_secs(300)));
        assert_eq!(
            effective,
            Some(Duration::from_secs(295)),
            "模型没传 timeout 时仍须有 deadline（外层 300s 减余量 5s）"
        );
        assert_eq!(deadline_secs(effective.unwrap()), 295);
    }

    /// 外层预算**没有**挂载 ⇒ 只有模型值约束（沿用既有语义）；两者皆无 ⇒ 无 deadline。
    #[test]
    fn test_effective_deadline_without_outer_budget_follows_the_model_value_only() {
        assert_eq!(
            effective_deadline(Some(Duration::from_secs(900)), None),
            Some(Duration::from_secs(900)),
            "没有外层预算时只受模型值约束"
        );
        assert_eq!(
            effective_deadline(None, None),
            None,
            "既无外层预算也无模型值 ⇒ 无 deadline（跑到自然结束）"
        );
    }

    /// 外层预算 < 余量 ⇒ 钳到**最小正秒数**，不得算出 0 或负数。
    #[test]
    fn test_effective_deadline_clamps_to_a_positive_minimum_when_outer_is_tighter() {
        let effective = effective_deadline(None, Some(Duration::from_secs(3)));
        let effective = effective.expect("外层预算存在 ⇒ 必有 deadline");
        assert_eq!(
            effective,
            Duration::from_secs(1),
            "外层预算小于余量时应钳到最小正秒数"
        );
        assert!(effective_secs_positive(effective));
    }

    /// 生效 deadline 必须是**正**的 —— 0 或负数会让超时立即触发或永不触发。
    fn effective_secs_positive(deadline: Duration) -> bool {
        !deadline.is_zero() && deadline_secs(deadline) >= 1
    }

    /// 生效 deadline 必须**严格早于**外层预算 —— 等于或大于外层，外层就会先赢
    /// （drop 工具 future ⇒ 只剩 `kill_on_drop`，孙进程成孤儿）。
    ///
    /// 外层预算小到减不出内层（不足 2ns）⇒ **无预算可用** ⇒ 不算 deadline。
    #[test]
    fn test_effective_deadline_is_strictly_earlier_than_the_outer_budget() {
        for outer_ms in [1u64, 3, 10, 100, 999, 1000, 5000, 6000, 60_000, 300_000] {
            let outer = Duration::from_millis(outer_ms);
            let effective = effective_deadline(None, Some(outer));
            let Some(effective) = effective else {
                panic!("外层预算 {outer_ms}ms ⇒ 须有生效 deadline");
            };
            assert!(
                effective > Duration::ZERO && effective < outer,
                "生效 deadline {effective:?} 必须是正的且严格早于外层预算 {outer:?}"
            );
        }
        // 外层不足 2ns ⇒ 取半后为 0 ⇒ 无预算可用（不得算出大于外层的值）
        let outer = Duration::from_nanos(1);
        assert_eq!(
            effective_deadline(None, Some(outer)),
            None,
            "外层预算不足 2ns ⇒ 减不出「正的且严格早于外层」的值 ⇒ 无预算可用"
        );
        // 有模型值时：外层视作「没有外层预算」⇒ 只受模型值约束
        assert_eq!(
            effective_deadline(Some(Duration::from_secs(5)), Some(outer)),
            Some(Duration::from_secs(5)),
            "外层预算不足 2ns ⇒ 视作没有外层预算"
        );
    }

    /// 模型传 `timeout: 900`、外层预算 6s ⇒ 生效 1s ⇒ **文案写 1**（不是 900）。
    ///
    /// 挂钟约 1s（不真等 295s）—— 断言落在「文案里的秒数」上。
    ///
    /// 变异：把文案改回写模型值 ⇒ 断言变红。
    #[tokio::test]
    async fn test_timeout_message_writes_the_effective_seconds_not_the_model_value() {
        let tool = BashTool::new(PathBuf::from("."));
        let ctx = ctx_with_budget(Duration::from_secs(6));
        let input = json!({ "command": "sleep 30", "timeout": 900 });

        let started = Instant::now();
        // 墙钟上界：工具内没有 deadline 时会挂到 `sleep 30` 自然结束 ⇒ 必须快速失败
        let err = tokio::time::timeout(TOOL_CALL_GUARD, tool.call(input, ctx))
            .await
            .expect("1s 后必须超时返回 Err")
            .expect_err("1s 后必须超时返回 Err");
        let elapsed = started.elapsed();

        assert!(matches!(err, ToolError::Timeout(_)), "got {err:?}");
        let msg = match &err {
            ToolError::Timeout(m) => m.clone(),
            _ => unreachable!(),
        };
        assert!(
            msg.contains("Command timed out after 1 seconds"),
            "文案须写生效值（1s），实际：{msg}"
        );
        assert!(!msg.contains("900"), "文案不得写模型传的值（900）：{msg}");
        assert!(
            elapsed < Duration::from_secs(3),
            "超时应按生效 deadline 收敛（~1s），实际耗时 {elapsed:?}"
        );
    }

    /// 模型**不传** `timeout` + 外层预算 6s ⇒ **无 boundary 路径**同样有 deadline。
    ///
    /// 完成条件是「**工具自己**发现 deadline 到点」⇒ 命令由工具终止
    /// （先杀进程组、再收尸），而不是等外层 drop 工具 future
    /// （那条路径只剩 `kill_on_drop`，孙进程会成孤儿）。
    ///
    /// 变异：删掉「未传值时取外层 − 余量」那一支 ⇒ 挂住（10s 兜底超时）⇒ 断言变红。
    #[tokio::test]
    async fn test_no_boundary_path_still_times_out_on_the_outer_budget_without_model_timeout() {
        let tool = BashTool::new(PathBuf::from("."));
        let ctx = ctx_with_budget(Duration::from_secs(6));
        let input = json!({ "command": "sleep 30" });

        let started = Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(10), tool.call(input, ctx))
            .await
            .expect("无 boundary 路径也必须有 deadline（10s 内必须返回）");
        let elapsed = started.elapsed();

        let err = result.expect_err("超时必须返回 Err");
        assert!(matches!(err, ToolError::Timeout(_)), "got {err:?}");
        let msg = match &err {
            ToolError::Timeout(m) => m.clone(),
            _ => unreachable!(),
        };
        assert!(
            msg.contains("Command timed out after 1 seconds"),
            "文案须写生效值（外层 6s 减余量 5s = 1s），实际：{msg}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "超时应按生效 deadline 收敛（~1s），实际耗时 {elapsed:?}"
        );
    }

    /// 有 boundary 路径用**同一份生效值** ⇒ 文案与无 boundary 路径逐字相同。
    #[tokio::test]
    async fn test_boundary_path_uses_the_same_effective_value() {
        let boundary = ys_protocol::QueueBoundarySource::new();
        let tool = BashTool::new(PathBuf::from("."));
        let ctx = ctx_with_boundary_and_budget(&boundary, Duration::from_secs(6));
        let input = json!({ "command": "sleep 30", "timeout": 900 });

        let started = Instant::now();
        // 墙钟上界：工具内没有 deadline 时会挂到 `sleep 30` 自然结束 ⇒ 必须快速失败
        let err = tokio::time::timeout(TOOL_CALL_GUARD, tool.call(input, ctx))
            .await
            .expect("1s 后必须超时返回 Err")
            .expect_err("1s 后必须超时返回 Err");
        let elapsed = started.elapsed();

        assert!(matches!(err, ToolError::Timeout(_)), "got {err:?}");
        let msg = match &err {
            ToolError::Timeout(m) => m.clone(),
            _ => unreachable!(),
        };
        assert!(
            msg.contains("Command timed out after 1 seconds"),
            "两条路径须用同一份生效值，实际：{msg}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "超时应按生效 deadline 收敛（~1s），实际耗时 {elapsed:?}"
        );
    }

    /// 模型传 `timeout: 0` ⇒ 生效 deadline 为 **0** ⇒ 文案逐字写 **0**。
    ///
    /// 合同是「文案写**生效值**」⇒ 生效值是 0 就写 0，不得抬到 1。
    ///
    /// 变异：把 `deadline_secs` 的取值改回「至少 1 秒」⇒ 文案变 "1 seconds" ⇒ 变红。
    #[tokio::test]
    async fn test_timeout_message_writes_zero_when_the_effective_deadline_is_zero() {
        let tool = BashTool::new(PathBuf::from("."));
        let ctx = make_ctx();
        let input = json!({ "command": "sleep 30", "timeout": 0 });

        let started = Instant::now();
        let err = tokio::time::timeout(TOOL_CALL_GUARD, tool.call(input, ctx))
            .await
            .expect("生效 deadline 为 0 ⇒ 必须立刻返回 Err（10s 内返回）")
            .expect_err("生效 deadline 为 0 ⇒ 必须返回 Err");
        let elapsed = started.elapsed();

        assert!(matches!(err, ToolError::Timeout(_)), "got {err:?}");
        let msg = match &err {
            ToolError::Timeout(m) => m.clone(),
            _ => unreachable!(),
        };
        assert!(
            msg.contains("Command timed out after 0 seconds"),
            "文案须逐字写生效值（0s），实际：{msg}"
        );
        assert!(
            !msg.contains("after 1 seconds"),
            "文案不得把 0 抬到 1：{msg}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "生效 deadline 为 0 ⇒ 必须立刻返回，实际耗时 {elapsed:?}"
        );
    }

    /// 「无 deadline」档位（挂 boundary、不挂 `tool_budget`、模型不传 `timeout`）
    /// ⇒ 正常命令必须**正常返回**：不误超时、输出完整、不挂死。
    ///
    /// 该档位的完成条件是「管道读到 EOF」⇒ 孙进程持着写端时会一直阻塞
    /// （见 `effective_deadline` 的文档）；本用例只钉「正常命令能返回」这一条。
    ///
    /// 变异：把阶段 A 的完成条件改成「不等 EOF」⇒ 输出被截断 ⇒ 断言变红；
    /// 或让该档位永不完成 ⇒ 10s 兜底超时 ⇒ 断言变红。
    #[tokio::test]
    async fn test_no_deadline_tier_returns_a_normal_command() {
        let boundary = ys_protocol::QueueBoundarySource::new();
        let tool = BashTool::new(PathBuf::from("."));
        let ctx = ctx_with(&boundary);
        assert!(
            ctx.tool_budget.is_none(),
            "本用例的前提是不挂 tool_budget（否则不是「无 deadline」档位）"
        );
        let input = json!({ "command": "echo no-deadline-tier" });

        let result = tokio::time::timeout(TOOL_CALL_GUARD, tool.call(input, ctx))
            .await
            .expect("无 deadline 档位的正常命令必须在 10s 内返回")
            .expect("无 deadline 档位的正常命令必须成功返回");

        assert!(!result.is_error, "正常命令不该报错：{}", result.content);
        assert!(
            result.content.contains("no-deadline-tier"),
            "正常命令的输出必须完整返回：{}",
            result.content
        );
    }
}
