//! 工具在**每轮固定前缀**里的文本贡献：snippet + guideline 块 —— 全部以 `ToolPrompt` 声明。
//!
//! # 为什么收拢成一处声明
//!
//! 加一个工具的连锁点原先是**两张手抄表**：`tool_snippet` 的 `match` 臂与 `render_guidelines`
//! 里的一条 `if tools.contains(..)`。两张表各写一遍工具名，漏改其一时编译通过、测试全绿，
//! 提示词静默退化成 `- newtool: Custom tool` 或丢掉整块 guideline。本模块是这两者的
//! **唯一**出处：每个工具一行 [`ToolPrompt`]，snippet 与 guideline 同处声明。
//!
//! # 两条彼此独立的序
//!
//! - **工具序**：`ToolPlugin::order` 升序（`toolset::builtin_plugins`）⇒ tools JSON 与
//!   `Available tools:` 清单的顺序。
//! - **guideline 序**：每块自带全局 [`GuidelineBlock::slot`]，与工具序**无关**。
//!
//! 两者不能合并：guideline 从 `edit` 开始而工具表从 `read` 开始；`read` 组的三块被 `grep`
//! 的块插在中间（`grep` 的门控嵌在 `read` 组的门控体内）。按工具序或插件序展开都会重排字节
//! ⇒ 每轮固定前缀变化 ⇒ 所有用户的 prompt cache 失效。
//!
//! # 归属工具只用于门控
//!
//! `ToolPrompt::tool` 是 `Gate::ThisTool` 的判定依据，**不是**渲染位置。跨工具门控
//! （`edit || write`、`read` 组四工具、`read || grep`、narrate 的七个工作型工具）由
//! [`Gate::AnyOf`] 表达，归属仍挂在其中一个工具名下（块必须先有声明者）。

use ys_tool::{Gate, GuidelineBlock, Scope, ToolPrompt};

use crate::prompt::SUBAGENT_SEND_DESCRIPTION;

/// guideline 块的**全局位次**。升序即渲染序；`slot` 一变，前缀字节就变。
///
/// 用命名常量而非裸数字：顺序是契约，散落的字面量让「插一块到中间」变成重编号整表。
pub(crate) mod slot {
    /// `edit` 组 ①：精确匹配。
    pub(crate) const EDIT_PRECISE: u16 = 0;
    /// `edit` 组 ②：一处多改合成一次调用。
    pub(crate) const EDIT_MULTI_LOCATION: u16 = 1;
    /// `edit` 组 ③：`oldText` 尽量短而唯一。
    pub(crate) const EDIT_SMALL_OLDTEXT: u16 = 2;
    /// `write`：只用于新文件或整体重写。
    pub(crate) const WRITE_ONLY_NEW: u16 = 3;
    /// `edit || write`：结构化写入，不用脚本。
    pub(crate) const WRITE_NOT_SCRIPTS: u16 = 4;
    /// `read` 组 ①：一条聚合命令产出完整答案。
    pub(crate) const EXPLORE_ONE_COMMAND: u16 = 5;
    /// `grep`：16 KiB 硬上限。
    pub(crate) const GREP_CAP: u16 = 6;
    /// `read` 组 ②：结构化检索工具的胜出场景。
    pub(crate) const READ_TOOLS_WIN: u16 = 7;
    /// `read || grep`：先撒网再下钻。
    pub(crate) const START_WIDE: u16 = 8;
    /// 七个工作型工具任一在场：过程叙述（仅主 agent）。
    pub(crate) const NARRATE: u16 = 9;
    /// `websearch`。
    pub(crate) const WEBSEARCH: u16 = 10;
    /// `webfetch`。
    pub(crate) const WEBFETCH: u16 = 11;
    /// `bash_bg` ①：长命令后台化、不轮询。
    pub(crate) const BASH_BG_RUN: u16 = 12;
    /// `bash_bg` ②：禁 `sleep` 等待。
    pub(crate) const BASH_BG_NO_SLEEP: u16 = 13;
    /// `subagent_async` ①：并发槽位数。
    pub(crate) const SUBAGENT_PARALLELISM: u16 = 14;
    /// `subagent_async` ②：结果自动送达、不轮询。
    pub(crate) const SUBAGENT_NO_POLL: u16 = 15;
    /// `subagent_async` ③：结果到达即汇报。
    pub(crate) const SUBAGENT_REPORT: u16 = 16;
    /// `subagent_async` ④：成批派发才并发。
    pub(crate) const SUBAGENT_BATCH: u16 = 17;
    /// `subagent_async` ⑤：`prompt` 四要素。
    pub(crate) const SUBAGENT_PROMPT_PARTS: u16 = 18;
    /// `subagent_async` ⑥：依赖不得靠「等一下」。
    pub(crate) const SUBAGENT_DEPENDENCIES: u16 = 19;
    /// `subagent_async` ⑦：错误/取消的诊断。
    pub(crate) const SUBAGENT_DIAGNOSTIC: u16 = 20;
    /// `subagent_async` ⑧：子 agent 不免费。
    pub(crate) const SUBAGENT_COST: u16 = 21;
    /// `subagent_async` ⑨：成批派发探索。
    pub(crate) const SUBAGENT_EXPLORE: u16 = 22;
    /// `subagent_send`：给仍在跑的子 agent 插话。
    pub(crate) const SUBAGENT_SEND: u16 = 23;

    /// `todo` 清单 —— 父表追加段末位（在 `subagent_send` 之后）。
    pub(crate) const TODO_MAINTAIN: u16 = 24;
}

/// 把多行字面量折成一个 `String` 块（`lines` 的每一项渲染成一行 `- {line}`）。
fn block(slot: u16, gate: Gate, scope: Scope, lines: &[&str]) -> GuidelineBlock {
    GuidelineBlock::new(
        slot,
        gate,
        scope,
        lines.iter().map(|line| (*line).to_string()).collect(),
    )
}

/// 单工具门控、主/子 agent 都渲染的块 —— 最常见的形态。
fn this_tool(slot: u16, lines: &[&str]) -> GuidelineBlock {
    block(slot, Gate::ThisTool, Scope::Always, lines)
}

/// 同 [`this_tool`]，但正文需要运行期插值（并发槽位数来自 `ys-component` 常量）。
fn this_tool_owned(slot: u16, lines: Vec<String>) -> GuidelineBlock {
    GuidelineBlock::new(slot, Gate::ThisTool, Scope::Always, lines)
}

/// narrate 块的门控成员：**工作型**工具（读、检索、执行、写）。
///
/// 纯联网 / 图像 / 派发工具不在内 —— 叙述服务于「多轮工具任务里用户看不到模型的思考」，
/// 只有这些工具会开启多轮工具序列。
const NARRATE_TOOLS: &[&str] = &["read", "grep", "find", "ls", "bash", "edit", "write"];

/// `read` 组的门控成员：四个只读检索工具。
const READ_GROUP: &[&str] = &["read", "grep", "find", "ls"];

/// 全部内置工具的 prompt 声明，**按工具名索引**。
///
/// 覆盖 14 个工具：`builtin_plugins` 的 11 条 + 父表追加段的 3 个
/// （`bash_bg` / `subagent_async` / `subagent_send`）。追加段工具不在
/// [`crate::toolset::builtin_plugins`] 里（它们由 `assembly` 造父表时追加），
/// 但**同样**要进 `Available tools:` 清单与 guideline 展开。
pub(crate) fn builtin_prompts() -> &'static [ToolPrompt] {
    static PROMPTS: std::sync::OnceLock<Vec<ToolPrompt>> = std::sync::OnceLock::new();
    PROMPTS.get_or_init(build)
}

/// 按工具名取声明（`snippet` 查表与 guideline 展开共用）。
pub(crate) fn prompt_for(name: &str) -> Option<&'static ToolPrompt> {
    builtin_prompts().iter().find(|p| p.tool == name)
}

fn build() -> Vec<ToolPrompt> {
    vec![
        ToolPrompt::new("read", "Read file contents").with_guidelines(vec![
            block(
                slot::EXPLORE_ONE_COMMAND,
                Gate::AnyOf(READ_GROUP),
                Scope::Always,
                &[
                    "Explore with one command, not one round per look. When the question is a \
                     lookup or an aggregation across many files — \"find every X\", \"count Y per \
                     crate\", \"show all Z with line numbers\" — plan the whole retrieval and write \
                     ONE command that emits the complete answer: a `for` loop over paths, \
                     `rg -n --context 2`, an `awk`. Every message is a full model round-trip, so \
                     ten small queries cost ten round-trips for what one command produces at once. \
                     `rg` respects .gitignore; bound the output with `-m`, `-l`, or `| head -n`.",
                ],
            ),
            block(
                slot::READ_TOOLS_WIN,
                Gate::AnyOf(READ_GROUP),
                Scope::Always,
                &[
                    "Reach for read/grep/find/ls when the next step genuinely depends on what you \
                     just saw, when you need offset/limit continuation on a single file, or when \
                     one targeted lookup is already the whole answer — `read paths=[...]` covers a \
                     handful of known files in one call.",
                ],
            ),
            block(
                slot::START_WIDE,
                Gate::AnyOf(&["read", "grep"]),
                Scope::Always,
                &[
                    "Start wide when you don't know where the answer is: one sweeping call (a \
                     recursive file listing, or a single grep for what you're after) beats several \
                     probing ones — then read what you found several files at a time. Keep line \
                     ranges narrow to hold a result's size down, not as the default",
                ],
            ),
            block(
                slot::NARRATE,
                Gate::AnyOf(NARRATE_TOOLS),
                Scope::MainOnly,
                &[
                    "Before the first tool call of a new request, state in **one short sentence** \
                     what you are about to do — the user must see the intent before any tool fires. \
                     After that, narrate only at milestones (target changed, conclusion reached, or \
                     blocked) — not before every tool batch. Each update is **one short sentence** \
                     (hard limit 120 characters) saying what you did or will do, nothing else. \
                     Never state hypotheses or reasoning (\"maybe\", \"I suspect\", \"let me check \
                     whether A or B\") — those are internal and cost the user a large re-read on \
                     every later round",
                ],
            ),
        ]),
        ToolPrompt::new("write", "Create or overwrite files").with_guidelines(vec![this_tool(
            slot::WRITE_ONLY_NEW,
            &["Use write only for new files or complete rewrites"],
        )]),
        ToolPrompt::new("edit", "Make precise file edits with exact text replacement")
            .with_guidelines(vec![
                this_tool(
                    slot::EDIT_PRECISE,
                    &["Use edit for precise changes (edits[].oldText must match exactly)"],
                ),
                this_tool(
                    slot::EDIT_MULTI_LOCATION,
                    &[
                        "When changing multiple separate locations in one file, use one edit call \
                         with multiple entries",
                    ],
                ),
                this_tool(
                    slot::EDIT_SMALL_OLDTEXT,
                    &[
                        "Keep edits[].oldText as small as possible while still being unique in the \
                         file",
                    ],
                ),
                block(
                    slot::WRITE_NOT_SCRIPTS,
                    Gate::AnyOf(&["edit", "write"]),
                    Scope::Always,
                    &[
                        "Write with edit and write, never with scripts: one edit call per file \
                         (multiple entries for multiple locations in that same file), one write \
                         call per new file. Splitting writes across several calls is fine — a \
                         scripted mutation skips the exact-match check.",
                    ],
                ),
            ]),
        ToolPrompt::new("bash", "Execute bash commands"),
        ToolPrompt::new(
            "grep",
            "Search file contents for a pattern (respects .gitignore when ripgrep is available)",
        )
        .with_guidelines(vec![this_tool(
            slot::GREP_CAP,
            &[
                "The `grep` tool caps its result at 16 KiB regardless of `limit`: when it \
                 truncates, narrow with `glob`, `path`, or a tighter pattern instead of raising \
                 `limit`.",
            ],
        )]),
        ToolPrompt::new("find", "Search for files by glob pattern"),
        ToolPrompt::new("ls", "List directory contents"),
        ToolPrompt::new(
            "ocr",
            "Extract the text in an image together with its pixel positions; set word_boxes for \
             per-word coordinates",
        ),
        ToolPrompt::new(
            "view_image",
            "View a local image file from the filesystem when visual inspection is needed. Use this \
             for images already available on disk; use `ocr` instead when you need the text in the \
             image together with its pixel coordinates. URLs are not accepted.",
        ),
        ToolPrompt::new(
            "websearch",
            "Search the web (tries the configured backends in a fixed order, stops at the first one \
             that answers)",
        )
        .with_guidelines(vec![this_tool(
            slot::WEBSEARCH,
            &["Use websearch for information outside the repository (API docs, error meanings)"],
        )]),
        ToolPrompt::new(
            "webfetch",
            "Fetch a URL as Markdown saved to a file, returning the head plus the full path \
             (continue with read offset= or grep, or pass prompt for a summary)",
        )
        .with_guidelines(vec![this_tool(
            slot::WEBFETCH,
            &[
                "Use webfetch to save a URL's content to a local file and get back only its head — \
                 read that path (read offset=) or grep it for more, and do not refetch the same \
                 URL. Pass prompt only when you need an answer rather than the page (e.g. \"how is \
                 the auth header built?\"): it costs one extra model call and is slower",
            ],
        )]),
        // 父表追加段：`bash_bg`（后台命令）。三条文案都只引用它，故单工具门控。
        ToolPrompt::new(
            "bash_bg",
            "Start a shell command in the background and get a handle back immediately; the \
             command keeps running while you do other work",
        )
        .with_guidelines(vec![
            this_tool(
                slot::BASH_BG_RUN,
                &[
                    "Run a command that takes more than a few seconds with bash_bg, then keep \
                     working on your own part — its result is delivered to you automatically when \
                     it finishes. Do NOT poll for it. If the turn ends first, the result arrives at \
                     the start of your next turn",
                ],
            ),
            this_tool(
                slot::BASH_BG_NO_SLEEP,
                &[
                    "Never use sleep to wait for something: start it with bash_bg; you will be \
                     notified when it finishes",
                ],
            ),
        ]),
        ToolPrompt::new(
            "subagent_async",
            "Start a sub-agent in the background and get a handle immediately; its result arrives \
             automatically when it finishes",
        )
        .with_guidelines(vec![
            this_tool_owned(
                slot::SUBAGENT_PARALLELISM,
                vec![format!(
                    "At most {} subagents run in parallel in one turn; extra calls queue. Dispatch \
                     only as many independent sub-tasks as you actually have.",
                    ys_component::DEFAULT_MAX_PARALLEL_TOOLS
                )],
            ),
            this_tool(
                slot::SUBAGENT_NO_POLL,
                &[
                    "Use subagent_async to keep big, self-contained investigations out of this \
                     context: it returns a handle immediately, the sub-agent does NOT see this \
                     conversation, so `prompt` must be fully self-contained, and its result is \
                     delivered to you automatically when it finishes — do NOT poll for it. Finish \
                     your own part and let the turn end; the result arrives as a new turn. Never \
                     claim a sub-agent is still running and never re-dispatch the same work",
                ],
            ),
            this_tool(
                slot::SUBAGENT_REPORT,
                &[
                    "When a sub-agent's result arrives, say in one line what it found and what you \
                     are doing next, then continue",
                ],
            ),
            this_tool(
                slot::SUBAGENT_BATCH,
                &[
                    "Scale the dispatch: one subagent is enough for a simple lookup. For genuinely \
                     independent sub-tasks, send ALL the `subagent_async` calls in ONE message — a \
                     single batch runs concurrently, whereas one call per round runs them one after \
                     another (N times slower)",
                ],
            ),
            this_tool(
                slot::SUBAGENT_PROMPT_PARTS,
                &[
                    "Write each `prompt` with four parts: goal, expected output format, \
                     sources/tools to use, and explicit boundaries — vague prompts make the \
                     sub-agents repeat each other's work",
                ],
            ),
            this_tool(
                slot::SUBAGENT_DEPENDENCIES,
                &[
                    "Before dispatching multiple subagents, check their dependencies and write \
                     scopes. If a task consumes another task's output or verifies its changes, \
                     dispatch it only after the prerequisite's result arrives. Run tasks in parallel \
                     only when their outputs are independent and their write scopes do not overlap; \
                     serialize them when the scope is unclear",
                ],
            ),
            this_tool(
                slot::SUBAGENT_DIAGNOSTIC,
                &[
                    "If a subagent returns an error or is cancelled, read its diagnostic first, \
                     then either narrow the task and retry or report the failure to the user",
                ],
            ),
            this_tool(
                slot::SUBAGENT_COST,
                &[
                    "A subagent is not free: it is another inference call plus another system \
                     prompt — use it to save context, not to look busy",
                ],
            ),
            this_tool(
                slot::SUBAGENT_EXPLORE,
                &[
                    "Delegate exploration only when you have several independent questions that \
                     each need many rounds: send them in ONE message as `subagent_async` with \
                     `mode=\"explore\"` so they run in parallel, then trust their `path:line` \
                     findings. For one or two small lookups, search directly — an agent's cold \
                     start costs more than the lookup itself.",
                ],
            ),
        ]),
        ToolPrompt::new("subagent_send", SUBAGENT_SEND_DESCRIPTION).with_guidelines(vec![
            this_tool(
                slot::SUBAGENT_SEND,
                &[
                    "Use subagent_send to steer a sub-agent that is still running: tell it to \
                     converge, add a constraint, or change direction, addressing it with the handle \
                     subagent_async returned (e.g. `sa3.2`). Your message reaches it at its next \
                     round boundary, not immediately, and does not interrupt the tool it is \
                     running. Any wording is fine, but it never replaces dispatching new work",
                ],
            ),
        ]),
        // 父表追加段末位：`todo` 清单。
        //
        // 清单只给主 agent（子表不含 `todo` ⇒ 子 agent 结构上拿不到），故 `Gate::ThisTool`
        // 在子提示里自然不命中 —— 无需额外的 `Scope::MainOnly`。
        ToolPrompt::new(
            "todo",
            "Maintain the task checklist: pass the COMPLETE list every call; mark abandoned \
             items `cancelled` instead of deleting them",
        )
        .with_guidelines(vec![this_tool(
            slot::TODO_MAINTAIN,
            &[
                "Use `todo` for work spanning several steps: write the checklist before starting, \
                 keep exactly one item `in_progress`, and mark each item `completed` as you finish \
                 it. Items are never deleted — an item you will not do is marked `cancelled`, \
                 because dropping an open item would silently close the loop that keeps you \
                 advancing",
            ],
        )]),
    ]
}
