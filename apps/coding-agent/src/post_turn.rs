//! 回合结束后的追问判据（一期「跨回合推进」，设计见
//! `docs/arch/cross-turn-advance/design.md` §3）。
//!
//! 三个纯函数按「回合前记 `HEAD` → 跑回合 → 判据 → 至多一次追问」的顺序被
//! 调用点串起来：
//!
//! - [`git_head`] 读取外部可观测事实（`git rev-parse HEAD`）
//! - [`should_follow_up`] 决定要不要再来一轮
//! - [`harness_input_text`] 给出追问回合的占位正文
//!
//! # 判据只认外部事实
//!
//! 触发依据是 `HEAD` 前后不等，**不是**模型的「我做完了」（ADR-0041 撤回的是后者）。
//! 附带的正面副作用：用户自己的提交同样会被 `HEAD` 差异覆盖，不只 agent 的。
//!
//! # 追问至多一次
//!
//! 由**调用点的结构**保证：一个顺序 `if`，没有计数器也没有循环。本模块因此不持有
//! 「是否已追问过」的任何状态 —— 本模块的三个函数全是纯函数。
use std::path::Path;
use std::process::Command;

/// `git rev-parse HEAD`；非 git 仓库 / 无 git / 命令失败 → `None`。
///
/// 非 git 仓库返回 `None` 是**正常情形**（护栏 ③：静默跳过，不报错）。若这里返回
/// `Result`，调用方就必须为「工作区不是 git 仓库」写一条错误分支，而它并非错误 ——
/// 选 `None` 让「检测不可用」与「检测表明没变」共用同一条静默路径，成本最低。
///
/// `workspace` 为 `None`（未绑定工作区）时**不 spawn 进程**：没有目录就无从定位。
///
/// **同步阻塞**：它 spawn 一个子进程。需要放进 async 上下文时由调用方决定是否
/// 包 `spawn_blocking`。
pub fn git_head(workspace: Option<&Path>) -> Option<String> {
    let dir = workspace?;
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        // 父进程若导出 `GIT_DIR` / `GIT_WORK_TREE`，它们会覆盖 `-C <dir>` 的定位，
        // 把命令指到别处 —— 显式移除，保证只认 `-C`。
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if head.is_empty() { None } else { Some(head) }
}

/// 是否该在正常收场后追加一个 harness 回合。
///
/// 三个条件**全部**成立才追问：
///
/// 1. 开关打开（`enabled`，默认关 —— 追加回合要花真钱）
/// 2. 回合**正常收场**：`outcome` 为 `Ok` 且 `stop_reason == Completed`
/// 3. `HEAD` 前后不等：`head_before` / `head_after` **都有值**且不同
///
/// 条件 2 排除了 `Cancelled` / `Truncated` / `MaxRounds` / `NoOutput` / `Failed`，
/// 以及 `Err`：非正常收场不该再追加成本（「用户中止永远赢」，与 ADR-0041 确立的
/// 原则一致）。
///
/// 条件 3 的「都有值」一半是护栏 ③：任一侧为 `None` 表示检测不可用（非 git 仓库、
/// 未装 git），此时**静默跳过**，既不追问也不报错。
pub fn should_follow_up(
    outcome: &Result<ys_loop::RunResult, ys_loop::LoopError>,
    head_before: Option<&str>,
    head_after: Option<String>,
    enabled: bool,
) -> bool {
    if !enabled {
        return false;
    }
    let result = match outcome {
        Ok(result) => result,
        // 模型 / 工具失败：不追加回合。
        Err(_) => return false,
    };
    // `StopReason` 是 `#[non_exhaustive]`，通配臂同时兜住未来新增的变体 ——
    // 未知收场一律按「非正常」处理，绝不多花钱。
    match result.stop_reason {
        ys_core::StopReason::Completed => {}
        _ => return false,
    }
    let (Some(before), Some(after)) = (head_before, head_after) else {
        return false;
    };
    before != after
}

/// 追问回合的占位输入。
///
/// 它**不是用户说的话** ⇒ `Role::Harness`（`crates/ys-core/src/message.rs:11`）。
/// `Role` 只是本地来源标记：TUI 据它画 System 行，模型出站时适配器映射成 user 角色
/// ⇒ 模型看到的就是一条普通的用户输入，压缩计划也不因它而改变。
///
/// 正文与「后台命令 / 子 agent」的占位输入同一形状：开门见山标出 `[harness]` 来源，
/// 并说清这一轮的**唯一**目标与不做时的出口（无需改动则直接说明），避免模型
/// 为了「有产出」而改无关文件。
///
/// **措辞只说「历史变了」而不说「产生了提交」**：判据是 `HEAD` 前后不等，
/// 而 `git reset --hard`、`git checkout` 同样改 `HEAD` 却未必产生提交。
/// 写死「提交」会让模型去核查一个不存在的提交。
pub fn harness_input_text() -> String {
    "[harness] 本回合的 git 历史发生了变化，请检查文档（AGENTS.md 等）\
     是否需同步更新；无需改动则直接说明。"
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use ys_core::{StopReason, Usage};
    use ys_loop::{LoopError, RunResult};

    use super::{git_head, harness_input_text, should_follow_up};

    /// 进程内原子序号：`SystemTime::as_nanos()` 在 macOS 只到微秒级，并行测试会撞名。
    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// 造一个确定不属于任何 git 仓库的临时目录。
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "yushan-post-turn-test-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            tag
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    /// 造一个指定 `stop_reason` 的成功回合结果；其余字段取无意义的占位值。
    fn run_result(stop_reason: StopReason) -> RunResult {
        RunResult {
            stop_reason,
            usage: Usage::default(),
            cache_reading: None,
            compaction_usage: Usage::default(),
            rounds: 1,
            final_message: None,
        }
    }

    fn ok(stop_reason: StopReason) -> Result<RunResult, LoopError> {
        Ok(run_result(stop_reason))
    }

    #[test]
    fn git_head_none_workspace_is_none() {
        assert_eq!(git_head(None), None);
    }

    #[test]
    fn git_head_non_git_dir_is_none() {
        let dir = temp_dir("non-git");
        assert_eq!(git_head(Some(dir.as_path())), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_head_after_commit_is_some() {
        let dir = temp_dir("git");
        // 未装 git ⇒ 本用例无事可验，直接跳过（不装 git 的机器上 `None` 已是契约）。
        let init = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["init", "-q"])
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output();
        let Ok(init) = init else {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        if !init.status.success() {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let commit = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args([
                "-c",
                "user.name=yushan-test",
                "-c",
                "user.email=yushan-test@example.invalid",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "init",
            ])
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .expect("git commit");
        assert!(commit.status.success(), "git commit 应成功");

        let head = git_head(Some(dir.as_path()));
        assert!(
            head.as_deref().is_some_and(|h| !h.is_empty()),
            "提交后应有 HEAD，实际 {:?}",
            head
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn harness_input_text_names_source_and_exit() {
        let text = harness_input_text();
        // 正文标明来源，且给出「无需改动」的出口。
        assert!(text.starts_with("[harness]"), "正文应标出来源：{text}");
        assert!(text.contains("AGENTS.md"), "正文应点名要检查的文档：{text}");
        assert!(
            text.contains("无需改动则直接说明"),
            "正文应给出不做时的出口：{text}"
        );
    }

    #[test]
    fn follow_up_disabled_is_false() {
        assert!(!should_follow_up(
            &ok(StopReason::Completed),
            Some("aaa"),
            Some("bbb".to_string()),
            false,
        ));
    }

    #[test]
    fn follow_up_err_outcome_is_false() {
        let outcome: Result<RunResult, LoopError> = Err(LoopError::ConfigError("boom".into()));
        assert!(!should_follow_up(
            &outcome,
            Some("aaa"),
            Some("bbb".to_string()),
            true,
        ));
    }

    #[test]
    fn follow_up_non_completed_stop_reasons_are_false() {
        // 非正常收场一律不追问：它们是故障、截断、轮数用尽、无产出或用户取消。
        for stop_reason in [
            StopReason::Cancelled,
            StopReason::Truncated,
            StopReason::MaxRounds,
            StopReason::NoOutput,
            StopReason::Failed,
        ] {
            assert!(
                !should_follow_up(
                    &ok(stop_reason.clone()),
                    Some("aaa"),
                    Some("bbb".to_string()),
                    true,
                ),
                "{stop_reason:?} 不该追问"
            );
        }
    }

    #[test]
    fn follow_up_completed_with_head_change_is_true() {
        assert!(should_follow_up(
            &ok(StopReason::Completed),
            Some("aaa"),
            Some("bbb".to_string()),
            true,
        ));
    }

    #[test]
    fn follow_up_completed_without_head_change_is_false() {
        assert!(!should_follow_up(
            &ok(StopReason::Completed),
            Some("aaa"),
            Some("aaa".to_string()),
            true,
        ));
    }

    #[test]
    fn follow_up_head_before_none_is_false() {
        assert!(!should_follow_up(
            &ok(StopReason::Completed),
            None,
            Some("bbb".to_string()),
            true,
        ));
    }

    #[test]
    fn follow_up_head_after_none_is_false() {
        assert!(!should_follow_up(
            &ok(StopReason::Completed),
            Some("aaa"),
            None,
            true,
        ));
    }

    /// **端到端判据验证**：在一个**真实**的 git 仓库里走完
    /// 「记 HEAD → 提交 → 再记 HEAD → 判据」的全链。
    ///
    /// 这条用例的存在理由是坐实一个曾经的真缺陷：取点若用「工作树绑定」
    /// （共享工作区下为 `None`）而不取**回合真正的工作目录**，整条判据恒为 `false`，
    /// 此时纯函数测试全绿而功能 100% 不生效。故本用例故意从**目录**出发，
    /// 先断言 `git_head` 有值，再断言判据为真。
    #[test]
    fn follow_up_true_across_real_commit() {
        let dir = temp_dir("real-commit");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
        };
        // 未装 git ⇒ 本用例无事可验（`None` 已是契约），直接返回。
        let Ok(init) = git(&["init", "-q"]) else {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        if !init.status.success() {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }

        let before = git_head(Some(dir.as_path()));
        // 空仓库无 HEAD ⇒ 取点必然拿不到值；这是「检测不可用」而非缺陷。
        // 先落一个 commit 让 HEAD 存在，再当基准。
        std::fs::write(dir.join("a.txt"), "a").expect("写文件");
        let _ = git(&["add", "."]);
        let _ = git(&["commit", "-qm", "base"]);
        let base = git_head(Some(dir.as_path()));
        assert!(base.is_some(), "有提交后 HEAD 必须有值（否则判据会恒为假）");
        assert_eq!(before, None, "空仓库无 HEAD");

        // 基准 HEAD + 新提交 ⇒ 判据为真。
        std::fs::write(dir.join("b.txt"), "b").expect("写文件");
        let _ = git(&["add", "."]);
        let _ = git(&["commit", "-qm", "next"]);
        let after = git_head(Some(dir.as_path()));
        assert!(should_follow_up(
            &ok(StopReason::Completed),
            base.as_deref(),
            after,
            true,
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
