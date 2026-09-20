//! **T15 —— R8 对账：skill 目录段（`<available_skills>`）的真实 input token 成本**
//!
//! # 这是校准，不是判定
//!
//! 本文件**只做测量**：同一个请求跑两次 —— 一次**无** `<available_skills>` 段、
//! 一次**有** —— 打印两次的**真实** `Usage.input_tokens` 之差，以及
//! [`estimate_text_tokens`]（`ys-loop/src/token.rs`，全仓唯一的启发式估算器）对**同一段文本**
//! 的静态估算。
//!
//! **输出要人看。不得用静态估算替代真实 usage。** 若拿不到真实 usage（无凭证 / provider
//! 不回报），本文件**如实打印「未校准」**并跳过断言 —— 绝不用估算值顶上去（设计 T15 的措辞：
//! 性质 = 尽力而为，`#[ignore]` 且不阻塞任何任务）。
//!
//! `#[ignore]`：需要 `YUSHAN_API_BASE` + `YUSHAN_API_KEY`（见 `tests/e2e_tools.rs` 的同一约定）。
//! 运行方式：
//!
//! ```text
//! YUSHAN_API_BASE=https://api.deepseek.com YUSHAN_API_KEY=sk-xxx \
//!   cargo test -p ys-coding-agent --test e2e_skills -- --ignored --nocapture
//! ```
//!
//! # 为什么两次运行只差一个 skills 目录
//!
//! 两次用**同一个工作区**、同一份基础系统提示、同一套工具、同一句用户消息：
//! - 跑「无」时工作区里**没有** `.yushan/skills`；
//! - 跑「有」时**先落盘若干真实 `SKILL.md`**，再把目录段追加在系统提示**末尾**
//!   （与 `prompt::build_system_prompt` 的形状一致：空 catalogue 的输出是含 skill 输出的
//!   **逐字节前缀**）。
//!
//! 于是两次请求的差异**只**是那一段文本 ⇒ `input_tokens` 之差就是这段文本的真实成本。
//! 固定前缀进了 KV cache 也不影响本测量（两次都命中同样的前缀；我们比的是**总量**）。
//!
//! # 为什么要在这里重写目录段的渲染
//!
//! `apps/coding-agent` **只有 bin target、没有 `lib.rs`**，故 `prompt::skill_section` /
//! `skills::render_line` 这两个函数**在 `tests/` 里 import 不到**（集成测试不是 bin 的内部
//! 模块）。为了让「估算 vs 真实」比的是**同一段文本**，这里的 [`skill_section`] 与
//! [`render_line`] 是那两处的**镜像**：
//!
//! | 本文件 | 镜像自 | 锚点 | 与生产的差异 |
//! |---|---|---|---|
//! | [`skill_section`] | `apps/coding-agent/src/prompt.rs` 的 `fn skill_section` | 前言四句是承重的（指出 `read`、相对路径语义、能力边界），改动须同步 | 逐字一致 |
//! | [`render_line`] | `apps/coding-agent/src/skills.rs` 的 `fn render_line` | 一行格式 `- <command>: <description> (location: <absolute path>)` | ⚠️ **不含 `escape_xml`**（见下） |
//!
//! ⚠️ **`render_line` 镜像的边界（有意声明，不是疏漏）**：生产的 `render_line` 会对
//! `command` / `description` / `path` 三个字段做 XML 转义（`&` `<` `>` `"` `'`），
//! 本文件的镜像**不做**。故**仅当 [`FIXTURES`] 里所有 `description` 都不含那五个字符时**，
//! 本镜像与生产逐字一致 —— 今天的 fixture 满足这一条件（纯中文 + 括号 + 标点），量到的是同一段文本。
//! **日后若要加含 `&` / `<` / `>` / `"` / `'` 的 fixture，本镜像会静默量到另一段字符串**：
//! 要么同步补上等价的转义，要么按下面「偏离风险已知并接受」的口径打折看待。
//!
//! **偏离风险已知并接受**：若生产侧改了这两处文案而本文件没跟上，对账的**绝对值**会偏
//! （缺常量偏移），但「真实 vs 估算」的**量级对比**仍然成立 —— 这正是本次校准要回答的问题
//! （设计 §待核实：目录段到底值多少 token、预算安全阀该定多大）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ys_core::Usage;
use ys_event::NoopEventSink;
use ys_loop::token::estimate_text_tokens;
use ys_loop::AgentInput;
use ys_model_openai_compat::{
    compat::ProviderCompat, OpenAICompatibleConfig, OpenAICompatibleModel,
};
use ys_runtime::{AgentBuilder, AgentPorts};
use ys_session::MemorySession;
use ys_tools_basic::ReadTool;

/// 单次 `run_turn` 的上限（真实网络调用，给足余量）。
const TURN_TIMEOUT: Duration = Duration::from_secs(120);

/// 两次运行共用的用户消息：**一句话、不触发工具** ⇒ 恰好一轮模型调用 ⇒
/// `RunResult.usage` 就是「一次请求的完整 prompt」（system + tools + 这条消息）。
/// 若模型仍调了工具，`usage` 会跨轮累加，本测量随之失真（下面会打印 `rounds` 供人核对）。
const TASK: &str = "Reply with exactly the two characters: ok";

/// 与生产 `prompt.rs` 的 `skill_section` **逐字一致**的镜像（见文件头）。
///
/// 前言四句都是承重的（设计 §CC 兼容性 偏离 3 / R7）：指出用 `read` 打开 `location`、
/// 相对路径按 skill 自己的目录解析、以及本 runtime 没有 subagent / plan mode。
fn skill_section(lines: &str) -> String {
    format!(
        "<available_skills>\n\
         The following skills are available. Each entry is `- <command>: <description> (location: <absolute path>)`.\n\
         To use a skill, open its `location` with the read tool and follow the instructions in that file.\n\
         Relative paths inside a skill body (e.g. `references/...`, or a sibling skill) resolve \
         relative to that skill's own directory, not the current working directory.\n\
         This runtime has no subagents and no plan mode — ignore instructions that ask for them.\n\
         \n\
         {lines}\n\
         </available_skills>"
    )
}

/// `render_line` 的镜像（见文件头的**边界声明**）：**不含 `escape_xml`**，故仅当
/// [`FIXTURES`] 的 `description` 不含 `& < > " '` 时与生产逐字一致（今天满足）。
///
/// `location` 是承重的：零工具路线要求模型据此自己 `read` **绝对路径**。
fn render_line(command: &str, description: &str, path: &Path) -> String {
    format!("- {command}: {description} (location: {})", path.display())
}

/// 一份评测用 skill：目录名（= 命令名）+ description + 正文。
///
/// description 刻意写成**与真实语料同密度**的中文（含「当…时使用」触发从句）——
/// 估算器的 CJK 分支（每字 1.5 token）正是本次要校准的对象，ASCII 桩数据测不出偏差。
struct Fixture {
    command: &'static str,
    description: &'static str,
}

/// 评测语料的**真实 `SKILL.md`**（会真落到磁盘上，不是内存里的字符串）。
///
/// 12 条，正文长度参差；description 的写法照真实语料（`~/.claude/skills/*` 的实测形态：
/// 一句中文说明 + 括号里的触发条件）。
const FIXTURES: [Fixture; 12] = [
    Fixture {
        command: "git-status",
        description: "显示当前 Git 仓库状态：分支、未提交的改动与暂存区差异 (Use when: 想知道改了哪些文件、或在提交前检查工作区)",
    },
    Fixture {
        command: "arch-design",
        description: "多方案架构设计与选择：用并行子代理生成多个截然不同的候选架构，对比分析后推荐方案并记录决策 (Use when: 设计新的模块或系统架构、面对多个方案需要做选择、需要从不同角度探索设计空间)",
    },
    Fixture {
        command: "tdd",
        description: "测试驱动开发，红-绿-重构循环：先写失败的测试再实现 (Use when: 想要使用 TDD 构建功能或修复 bug、提到 red-green-refactor、需要集成测试或要求测试先行开发)",
    },
    Fixture {
        command: "diagnose",
        description: "针对疑难 Bug 和性能回退的规范化诊断循环：复现 → 最小化 → 假设 → 探测 → 修复 → 回归测试 (Use when: 用户报告 Bug、说某个东西坏了或抛出异常、或描述性能回退)",
    },
    Fixture {
        command: "handoff",
        description: "将当前对话压缩成交接文档，供另一个 agent 接手继续工作 (Use when: 需要把上下文交给另一个会话、或对话过长需要换手)",
    },
    Fixture {
        command: "deep-research",
        description: "深度研究harness：扇出网络搜索、抓取来源、对抗式核验结论、综合成带引用的报告 (Use when: 需要多来源、可核查的研究报告；若问题过于宽泛，先澄清 2-3 个关键问题再动手)",
    },
    Fixture {
        command: "grill-me",
        description: "反复深入地访谈用户的每个计划或设计，直到达成共同理解，解决决策树的每个分支 (Use when: 想压力测试某个计划、被拷问设计、或提到 grill me)",
    },
    Fixture {
        command: "caveman",
        description: "超精简沟通模式：通过去除填充词、冠词和客套话，将 token 消耗减少约 75%，同时保持完整的技术准确性 (Use when: 用户说 caveman mode、talk like caveman、be brief 或 less tokens)",
    },
    Fixture {
        command: "to-prd",
        description: "将当前对话上下文转为产品需求文档并发布到项目的 issue 跟踪器 (Use when: 想从当前上下文创建 PRD、或需要把讨论固化成可执行的需求说明)",
    },
    Fixture {
        command: "security-review",
        description: "对当前分支上的待提交改动做安全审查，覆盖注入、鉴权、密钥泄漏与依赖风险 (Use when: 提交前想做一次安全体检、或改动涉及认证与数据边界)",
    },
    Fixture {
        command: "request-refactor-plan",
        description: "通过用户访谈创建包含小提交的详细重构计划，然后将其提交为 GitHub Issue (Use when: 想规划重构、创建重构 RFC、或把重构拆分为安全的增量步骤)",
    },
    Fixture {
        command: "write-a-skill",
        description: "以正确的结构、渐进式披露和捆绑资源创建新的代理技能 (Use when: 想创建、编写或构建新技能时使用)",
    },
];

/// 进程内唯一临时目录：**原子序号 + `process::id()`**（并行安全，项目硬约定）。
fn unique_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "yushan_e2e_skills_{tag}_{}_{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 把评测语料的**真实 `SKILL.md`** 落到 `<workspace>/.yushan/skills/<command>/SKILL.md`，
/// 返回逐行的目录段（`render_line` 的产物，`\n` 连接）。
///
/// 落盘是**有意**的：本用例要的就是「真实 SKILL.md 存在时，生产启动期会注入什么」，
/// 而不是一串内存里的字面量（`-p` / `--json` / TUI 三模式共用同一份扫描结果）。
fn write_corpus(workspace: &Path) -> String {
    let root = workspace.join(".yushan").join("skills");
    let mut lines = Vec::with_capacity(FIXTURES.len());
    for fixture in &FIXTURES {
        let dir = root.join(fixture.command);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        // frontmatter 形态照抄真实语料：`name` + `description`，正文随各 skill 不同。
        std::fs::write(
            &path,
            format!(
                "---\nname: {}\ndescription: {}\n---\n\n# {}\n\n这是 {} 的正文占位。\
                 真实语料里这里是完整的操作指引，正文只在激活时读取，不进启动期内存。\n",
                fixture.command, fixture.description, fixture.command, fixture.command
            ),
        )
        .unwrap();
        lines.push(render_line(fixture.command, fixture.description, &path));
    }
    lines.join("\n")
}

/// 两次运行共用的基础系统提示（不含 skill 段）。
///
/// 刻意**短而固定**：它进两次请求的**同一位置**，故对本测量只贡献一个常量偏移。
/// 这里不镜像生产 `build_system_prompt` 的全文 —— 本文件要量的是**增量**，不是绝对值。
fn base_prompt(workspace: &Path) -> String {
    format!(
        "You are a minimal coding agent runtime.\n\
         Working directory: {}\n\
         Use the read tool to examine files. Be concise in your responses.",
        workspace.display()
    )
}

/// 跑一次真实 `run_turn`，返回 `(usage, rounds)`。
///
/// 工具表**两次相同**（只有 `read`）：工具 spec 也进 prompt，故它必须是常量，
/// 否则差值里会混进「工具面变化」这一项。
async fn measure(model: &OpenAICompatibleModel, workspace: &Path, prompt: &str) -> (Usage, u32) {
    let mut agent = AgentBuilder::new()
        .tool(ReadTool::new(workspace.to_path_buf()))
        .system_prompt(prompt)
        .build()
        .expect("failed to build agent");

    let mut session = MemorySession::new();
    let mut events = NoopEventSink;
    let result = tokio::time::timeout(
        TURN_TIMEOUT,
        agent.run_turn(
            AgentInput::text(TASK),
            AgentPorts::new(Some(model), &mut session, &mut events, None),
        ),
    )
    .await
    .expect("run_turn timed out")
    .expect("run_turn failed");

    (result.usage, result.rounds)
}

/// **R8 对账主用例。**
///
/// 同一工作区、同一请求跑两次；第二次之前才把 12 份真实 `SKILL.md` 落盘并把
/// `<available_skills>` 段追加到系统提示末尾。打印：
///
/// - 两次的**真实** `input_tokens` 与 `rounds`（rounds > 1 说明模型调了工具，测量失真）；
/// - 两者之差（= 该 skill 目录段的**真实**成本）；
/// - `estimate_text_tokens(段文本)`（**同一段文本**的静态估算），以及真实 / 估算的比值。
///
/// **断言只有一条，且刻意极宽**：`delta > 0`（目录段确实被送进了请求）。
/// 它不判断估算准不准 —— 那是人看数字后的事（校准，不是门槛）。
///
/// provider 不回报 usage（`input_tokens == 0`）时**不做断言**，只打印「未校准」。
#[tokio::test]
#[ignore] // 需要真实 API 凭证。运行：cargo test -p ys-coding-agent --test e2e_skills -- --ignored --nocapture
async fn e2e_skills_section_input_token_delta() {
    let api_base = std::env::var("YUSHAN_API_BASE")
        .or_else(|_| std::env::var("OPENAI_API_BASE"))
        .expect("Set YUSHAN_API_BASE or OPENAI_API_BASE env var");
    let api_key = std::env::var("YUSHAN_API_KEY")
        .or_else(|_| std::env::var("OPENAI_API_KEY"))
        .expect("Set YUSHAN_API_KEY or OPENAI_API_KEY env var");
    let model_name = std::env::var("YUSHAN_MODEL").unwrap_or_else(|_| "deepseek-chat".to_string());

    let workspace = unique_dir("delta");

    // 低温 + 固定的基础提示 ⇒ 两次请求的外部变量只剩「有没有 skill 段」。
    let model = OpenAICompatibleModel::new(OpenAICompatibleConfig {
        api_base,
        api_key,
        model: model_name.clone(),
        max_tokens: Some(4096),
        temperature: Some(0.0),
        compat: ProviderCompat::deepseek(),
    });

    // ── 跑 ①：**无** `<available_skills>`（工作区里此刻还没有 skills 目录）──
    let bare_prompt = base_prompt(&workspace);
    assert!(
        !workspace.join(".yushan").join("skills").exists(),
        "前置：第一次运行前工作区里不得有 skills 目录"
    );
    let (bare_usage, bare_rounds) = measure(&model, &workspace, &bare_prompt).await;

    // ── 落盘语料，构造含 skill 段的提示（段**追加在末尾**，与生产形状一致）──
    let lines = write_corpus(&workspace);
    let section = skill_section(&lines);
    let with_prompt = format!("{bare_prompt}\n\n{section}");
    assert!(
        with_prompt.starts_with(&bare_prompt),
        "含 skill 的提示必须是含 skill 之前的**逐字节前缀** + 追加段"
    );

    // ── 跑 ②：**有** `<available_skills>` ──
    let (with_usage, with_rounds) = measure(&model, &workspace, &with_prompt).await;

    // ── 对账输出（**给人看**，不是断言）──
    let estimate = estimate_text_tokens(&section);
    let delta = with_usage
        .input_tokens
        .saturating_sub(bare_usage.input_tokens);

    println!("\n──────── R8 对账：skill 目录段的 input token 成本 ────────");
    println!("model                = {model_name}");
    println!("skill 条数           = {} 条", FIXTURES.len());
    println!(
        "段字节数             = {} B（含 <available_skills> wrapper）",
        section.len()
    );
    println!("段字符数(CJK 占比)   = {} chars", section.chars().count());
    println!("───── 真实 usage（provider 回报）─────");
    println!(
        "无 skill 段 input    = {} token（rounds = {bare_rounds}）",
        bare_usage.input_tokens
    );
    println!(
        "有 skill 段 input    = {} token（rounds = {with_rounds}）",
        with_usage.input_tokens
    );
    println!("真实增量             = {delta} token");
    println!("───── 静态估算（estimate_text_tokens，同一段文本）─────");
    println!("估算增量             = {estimate} token");
    if estimate > 0 {
        println!(
            "真实 / 估算          = {:.3}",
            delta as f64 / estimate as f64
        );
    }
    println!(
        "（output_tokens：无 = {}，有 = {}）",
        bare_usage.output_tokens, with_usage.output_tokens
    );
    println!("──────────────────────────────────────────────────────\n");

    // ── 仅当 provider 真的回报了 usage 才做（极宽的）判定 ──
    if bare_usage.input_tokens == 0 || with_usage.input_tokens == 0 {
        println!(
            "⚠ 本次**未校准**：provider 未回报 usage（input_tokens = 0），\
             不得据此回看 Budget.total_bytes，也不得用上面的估算值顶替真实 usage。"
        );
        workspace_cleanup(&workspace);
        return;
    }
    if bare_rounds > 1 || with_rounds > 1 {
        println!(
            "⚠ 注意：rounds > 1 说明模型调用了工具 ⇒ usage 跨轮累加，本次增量**不纯**，\
             回看数据时须打折看待（可把 TASK 改得更不易触发工具后重跑）。"
        );
    }

    // 唯一的断言，刻意极宽：目录段确实进了请求（真实增量必须为正）。
    assert!(
        delta > 0,
        "含 <available_skills> 的请求 input_tokens 应大于不含的（真实增量 = {delta}）：\
         无 = {}，有 = {}",
        bare_usage.input_tokens,
        with_usage.input_tokens
    );

    workspace_cleanup(&workspace);
}

/// 自清理（测试不留临时目录）。
fn workspace_cleanup(workspace: &Path) {
    let _ = std::fs::remove_dir_all(workspace);
}
