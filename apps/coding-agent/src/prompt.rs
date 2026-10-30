use std::path::Path;

use ys_tool::ToolSpec;

use crate::skills::Catalogue;

/// 用工具片段和动态 guideline 构建 system prompt。
///
/// # 工具清单从 `tools` 派生
///
/// 清单**逐字按 `tools` 的顺序**渲染 `- {name}: {snippet}` —— 保序由工具表装配端
/// （`toolset::build_tools` → `ToolRegistry`，见
/// `docs/design-plans/2026-09-16-search-tools.md` B1）保证，本函数不排序、
/// 不查自己的名录：`tools` 里有什么就列什么。这样提示词与注册表**不可能漂移**。
///
/// # 段序
///
/// ```text
/// cwd 段（最前） → 角色段 → Available tools → Guidelines → <project_context> → skill 目录段（最末）
/// ```
///
/// skill 目录段（纯追加，设计 §选择理由 F）在**最末**，故 **空 `catalogue` 的输出是任何非空
/// `catalogue` 输出的逐字节前缀** —— 这是「同一台机器上先后两次启动的提示前缀稳定」的可测形式
/// （严格前缀不变量）。cwd 段**在最前**是实测结论（理由见 [`cwd_section`]）；它在前不在后，
/// **不影响**该不变量。
///
/// 门控（照 pi）：仅当工具表里**有 `read`**（skill 正文靠它加载）且目录非空时追加。
/// 空目录时**什么都不追加**（不留空行）。
///
/// 命中自定义 prompt（`.yushan/SYSTEM.md` / `~/.yushan/SYSTEM.md`）时整体短路返回，
/// **不追加** skill 段 —— 既有契约逐字保留（用户全权接管提示词）；代价（模型无法
/// 自主发现 skill）由 `main` 的启动期提示交代，见 [`has_custom_prompt`]。
pub fn build_system_prompt(cwd: &Path, tools: &[ToolSpec], catalogue: &Catalogue) -> String {
    // 1. 检查是否有自定义 prompt 文件
    if let Some(custom) = find_custom_prompt(cwd) {
        return custom;
    }

    // 2. 构建默认 prompt —— 清单顺序 == `tools` 顺序。
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    let tool_list: Vec<String> = names
        .iter()
        .map(|name| format!("- {name}: {}", tool_snippet(name)))
        .collect();

    let tool_list = tool_list.join("\n");
    let guidelines = tool_guidelines(&names);

    let mut head = format!(
        "You are a coding agent. You help users with software engineering tasks.\n\n\
         Available tools:\n{tool_list}\n\nGuidelines:\n{guidelines}"
    );

    // 3. 追加项目上下文（AGENTS.md / YUSHAN.md）
    let context_files = load_project_context(cwd);
    if !context_files.is_empty() {
        head.push_str("\n\n<project_context>\n\n");
        head.push_str("Project-specific instructions and guidelines:\n\n");
        for ctx in &context_files {
            head.push_str(&format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
                ctx.path.display(),
                ctx.content
            ));
        }
        head.push_str("</project_context>");
    }

    let cwd_block = cwd_section(cwd);

    // 4. skill 目录段 —— 门控：有 `read`（skill 正文靠它加载）且目录非空；否则**一字不加**。
    let skills_block = if tools.iter().any(|t| t.name == "read") && !catalogue.lines.is_empty() {
        // 能力边界声明**按工具表派生**：父表有派发工具 ⇒ 不说「no subagents」；
        // 子表没有 ⇒ 逐字保留（对子 agent 而言那句是真话）。
        Some(skill_section(
            &catalogue.lines,
            tools.iter().any(|t| t.name == "subagent"),
        ))
    } else {
        None
    };

    // 5. 拼装（段间一律 `\n\n`）：**cwd 段在最前**，skill 段在最末。
    //
    //    位置与措辞都是实测定的（`docs/arch/bash-cwd-contract/context.md` §实验）：
    //    cwd 段**单独**挪位置（最末 / 最前）对模型行为毫无影响 —— 三种位置都仍是 100% 的
    //    `cd <工作目录> &&` 前缀；只有在措辞把「相对路径的解析根」与「每次调用是新 shell」
    //    说全**之后**，位置才开始起作用：说全 + 置顶 = 0%，说全但留在 `<project_context>`
    //    之后 = 39%。合理解释是注意力（本仓的 `<project_context>` 是一整份 `AGENTS.md`）。
    let mut segments: Vec<String> = vec![cwd_block, head];
    if let Some(s) = skills_block {
        segments.push(s);
    }
    segments.join("\n\n")
}

/// 探索子 agent 的输出契约（纯常量）。
///
/// 与主 agent 的「过程叙述」相对：子 agent 的读者是父模型，不是用户 —— 它要的是
/// 可核对的结论，不是面向人的铺垫与建议。
///
/// **必须有界**：要求「`Not found:` 列出搜过哪里」会鼓励穷举 —— 要填满那一节
/// 就必须把每个角落都搜一遍。契约改为「一句结论 + 至多 5 条发现 + 一句未确认」，
/// 让交卷成本低于继续探索。
const EXPLORE_REPORT_FORMAT: &str = "Report format:\n\
- Answer the question in one sentence first (at most 2 lines).\n\
- Then at most 5 findings, one line each: `path:line — fact`.\n\
- If you could not confirm part of the question, say so in one line. Do not list every\n  \
place you looked.\n\
- No preamble, no restating the task, no recommendations, no file dumps.";

/// 探索子 agent 的**检索方法**（纯常量）。
///
/// 每条都对应一个可观测的轮数 / 成本契约：
/// - 定位与上下文用**一条**聚合命令（`rg -n --context`），而不是「先定位再读」——
///   每次消息都是一次完整模型往返，聚合命令把 10 轮降到 6 轮。
/// - `grep` 的结果有 **16 KiB 硬上限**：这是模型无法从工具描述推断的事实，
///   输出被截断时只能收窄模式 / 路径，加大 `limit` 不会返回更多。
/// - 只要文件名时用 `rg -l`：避免把正文搬进上下文。
/// - 只回答被问到的问题：无界审计会让探索失去终点；核对由调用方负责。
const EXPLORE_SEARCH_METHOD: &str = "How to search:\n\
- Locate and show context in ONE command (`rg -n --context 2 …`), not locate-then-read.\n\
- If output is truncated, narrow with `-g`, a path, or a tighter pattern. Raising `limit`\n  \
will not return more — grep caps its result at 16 KiB.\n\
- Use `rg -l` when you only need file names.\n\
- Answer the question that was asked; do not expand the audit beyond it. The caller verifies.";

/// 构建**探索子 agent** 的系统提示。
///
/// # 与 [`build_system_prompt`] 的差异
///
/// 1. 角色行改为探索语义（只读、带引用）；
/// 2. guidelines 去掉「过程叙述」那一条 —— 子 agent 的输出不面向用户；
/// 3. 追加 [`EXPLORE_REPORT_FORMAT`] 输出契约；
/// 4. **不加载项目上下文**（AGENTS.md / YUSHAN.md）。探索 agent 只需定位事实，
///    项目约定段（约 6.8k token）是每轮前缀成本，且会诱导它「按规范做事」而不是
///    「尽快定位」；主 agent 仍带着它。
///
/// 其余段（cwd、skill 目录）逐字沿用同一套渲染：本次对比只改「角色 + 输出契约 +
/// 工具集」这一个变量。
///
/// # 不走自定义 prompt 短路
///
/// `.yushan/SYSTEM.md` / `~/.yushan/SYSTEM.md` 是用户对**主 agent** 的接管；
/// 探索子 agent 需要自己的输出契约，命中自定义文件时短路会让它退化成主 agent 的
/// 提示，模式对比即失效。
pub fn build_explore_prompt(cwd: &Path, tools: &[ToolSpec], catalogue: &Catalogue) -> String {
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    let tool_list: Vec<String> = names
        .iter()
        .map(|name| format!("- {name}: {}", tool_snippet(name)))
        .collect();
    let tool_list = tool_list.join("\n");
    let guidelines = render_guidelines(&names, false);

    let head = format!(
        "You are an exploration agent. You investigate a codebase or its documents and report \
         findings with exact citations. You do not modify anything.\n\n\
         Available tools:\n{tool_list}\n\nGuidelines:\n{guidelines}\n\n{EXPLORE_SEARCH_METHOD}\n\n\
         {EXPLORE_REPORT_FORMAT}"
    );

    let cwd_block = cwd_section(cwd);
    let skills_block = if tools.iter().any(|t| t.name == "read") && !catalogue.lines.is_empty() {
        Some(skill_section(
            &catalogue.lines,
            tools.iter().any(|t| t.name == "subagent"),
        ))
    } else {
        None
    };

    let mut segments: Vec<String> = vec![cwd_block, head];
    if let Some(s) = skills_block {
        segments.push(s);
    }
    segments.join("\n\n")
}

/// cwd 段 —— **整条提示的第一段**。
///
/// # 只有事实，没有禁令
///
/// 刻意不写「不要 cd」：pi 的对应行也只有事实；且本轮的既定路线是「陈述事实 +
/// 事实不成立时硬报错」（不是靠训话约束模型）。
///
/// # 为什么是两句，而不是一句
///
/// 实测（同一任务型、同一模型，见上面 §实验）：只写「当前工作目录是 X」时，这一段
/// **放在哪里都一样** —— 最前、`<project_context>` 之后、最末，模型都以 `cd <工作目录> &&`
/// 开头（100%）。补上后两条事实并置顶后降到 **0%**。缺的那两条正是模型真正需要知道的：
/// **相对路径的解析根**（否则它自己钉一个绝对路径）、**每次调用是新 shell**
/// （否则它以为 `cd` 能留到下一次调用）。
fn cwd_section(cwd: &Path) -> String {
    format!(
        "Current working directory: {}\n\
         Relative paths in every tool resolve against it, and each `bash` call starts \
         there in a fresh shell.",
        cwd.display()
    )
}

/// 是否命中自定义 prompt（`.yushan/SYSTEM.md` 项目级，或 `~/.yushan/SYSTEM.md`
/// 用户级）—— 供 `main` 的启动期提示用。
///
/// 薄包装 [`find_custom_prompt`]：只回答「有没有」，**不把它改 pub**（该函数返回
/// 内容，暴露出去会诱使调用方绕过 [`build_system_prompt`] 的短路语义）。
pub fn has_custom_prompt(cwd: &Path) -> bool {
    find_custom_prompt(cwd).is_some()
}

/// 渲染 `<available_skills>` 段。
///
/// 前言里的三句话都是**承重**的（设计 §CC 兼容性 偏离 3 / R7）：
///
/// 1. **用 `read` 打开 `location`** —— 零工具路线（ADR-0019 决策 1）的唯一触发线索；
///    实测模型在给出绝对路径时会首跳即 `read`（R1 已验证）。
/// 2. **相对路径按 skill 自己所在目录解析** —— 实测多条正文按相对路径交叉引用兄弟
///    skill（如 `../design-interface/SKILL.md`），按 CWD 解析必然找不到。
/// 3. **能力边界声明**（无 subagent / plan mode）—— 正文里约 20 处 Claude 专有工具名
///    在本 runtime 静默失效；不做符号映射（那是把上游知识写死），改为声明边界。
fn skill_section(lines: &str, has_subagent: bool) -> String {
    let capability = if has_subagent {
        "This runtime CAN delegate tasks to subagents via the `subagent` tool; it has no plan mode — ignore instructions that ask for plan mode."
    } else {
        "This runtime has no subagents and no plan mode — ignore instructions that ask for them."
    };
    format!(
        "<available_skills>\n\
         The following skills are available. Each entry is `- <command>: <description> (location: <absolute path>)`.\n\
         To use a skill, open its `location` with the read tool and follow the instructions in that file.\n\
         Relative paths inside a skill body (e.g. `references/...`, or a sibling skill) resolve \
         relative to that skill's own directory, not the current working directory.\n\
         {capability}\n\
         \n\
         {lines}\n\
         </available_skills>"
    )
}

/// 工具的一句话描述，按 **name** 查表。
///
/// 名字不在表里时回退到 `"Custom tool"`（外部工具 / 未来新增工具都能列出而不报错）。
fn tool_snippet(name: &str) -> &'static str {
    match name {
        "read" => "Read file contents",
        "write" => "Create or overwrite files",
        "edit" => "Make precise file edits with exact text replacement",
        "bash" => "Execute bash commands",
        // 后台命令的启动工具（`docs/arch/bg-notify/design.md` 的契约文案）。
        // 清单里必须说清「后台跑 + 立即给句柄」—— 否则这一行会退化成兜底文案
        // （`Custom tool`），模型看不出它与 `bash` 的差别。
        "bash_bg" => {
            "Start a shell command in the background and get a handle back immediately; the command keeps running while you do other work"
        }
        "grep" => {
            "Search file contents for a pattern (respects .gitignore when ripgrep is available)"
        }
        "find" => "Search for files by glob pattern",
        "ls" => "List directory contents",
        // 与 `WebSearchTool` / `WebFetchTool` 的 `spec().name` 逐字一致（装配端名字是契约）。
        "websearch" => {
            "Search the web (tries the configured backends in a fixed order, stops at the first one that answers)"
        }
        // 末尾一句是**可选** `prompt` 参数的唯一线索：按意图取摘要而非整页原文。
        "webfetch" => {
            "Fetch a URL as Markdown saved to a file, returning the head plus the full path (continue with read offset= or grep, or pass prompt for a summary)"
        }
        // 派发工具（ADR-0022 决策 18 定的名字，装配端 `spec().name` 是契约）。
        "subagent" => {
            "Delegate a self-contained task to an isolated sub-agent and get back a summary plus a transcript path"
        }
        _ => "Custom tool",
    }
}

fn tool_guidelines(tools: &[&str]) -> String {
    render_guidelines(tools, true)
}

/// guideline 列表的**唯一**渲染处。
///
/// `narrate = false` 只去掉「过程叙述」那一条：它服务于面向用户的进展可见性，
/// 而子 agent 的输出只回给父模型。其余条目按工具表门控，两种提示共享同一份文案。
fn render_guidelines(tools: &[&str], narrate: bool) -> String {
    let mut guidelines: Vec<String> = Vec::new();

    if tools.contains(&"edit") {
        guidelines
            .push("Use edit for precise changes (edits[].oldText must match exactly)".to_string());
        guidelines.push(
            "When changing multiple separate locations in one file, use one edit call with multiple entries".to_string(),
        );
        guidelines.push(
            "Keep edits[].oldText as small as possible while still being unique in the file"
                .to_string(),
        );
    }
    if tools.contains(&"write") {
        guidelines.push("Use write only for new files or complete rewrites".to_string());
    }
    // 写入走**结构化工具**，不走脚本：`edit` 的 `edits[].oldText` 精确匹配是变更安全性的
    // 唯一校验点，而 `sed` / `awk` / `python -c` 之类的脚本化写入绕过了它 —— 匹配失败、
    // 改错位置都不会被拦住。拆成多次调用本身没有代价（每次仍是一次往返），故不必为了
    // 「少调几次」而牺牲校验。
    //
    // 门控：`edit` / `write` 任一在场才渲染 —— 这条文案只引用这两个工具，缺了就成悬空指令。
    if tools.contains(&"edit") || tools.contains(&"write") {
        guidelines.push(
            "Write with edit and write, never with scripts: one edit call per file (multiple \
             entries for multiple locations in that same file), one write call per new file. \
             Splitting writes across several calls is fine — a scripted mutation skips the \
             exact-match check."
                .to_string(),
        );
    }
    // 探索走**一条聚合命令产出完整答案**，不走逐轮小查询：问题若是跨多文件的查找或聚合
    // （「找出每个 X」「按 crate 统计 Y」「带行号列出所有 Z」），应规划整次检索、用一条命令
    // 直接吐出完整答案（对路径的 `for` 循环、`rg -n --context 2`、`awk`）。每条消息都是一次
    // 完整的模型往返，故十条小查询要付十次往返，而一条命令一次产出。
    //
    // 契约的核心是**强制综合**：命令必须自己完成筛选与汇总，而不是只把素材捞回来；它不限制
    // 调用数 —— 需要串联的探索仍可多步进行。
    //
    // 不写成「把独立命令放进同一条消息」：实测该措辞会被理解成「多跑几条命令」，模型转而做
    // 广度枚举而非集中验证，总调用数 +33%、轮数 +35%，而「调用/轮」比例几乎不变
    // （1.50 → 1.48）—— 增加的是命令总量，不是每次往返的产出。
    //
    // `.gitignore` 语义与有界输出是聚合命令必须自己守的两点：`rg` 默认尊重 .gitignore，
    // 用 `-m` / `-l` / `| head -n` 截断，避免一次聚合把上下文撑爆。
    //
    // 提示词不写「用 read/grep/find/ls 代替 bash」这类逐工具禁令：它们会把本可聚合的
    // 检索拆成逐个结构化调用，正是轮数膨胀的来源；结构化工具的胜出场景由下一条正向陈述。
    //
    // 门控：只读工具（`read` / `grep` / `find` / `ls`）任一在场才渲染 —— 文案引用 `rg` 与
    // `read paths=[...]`，没有这些工具时模型无从执行。
    if tools
        .iter()
        .any(|t| matches!(*t, "read" | "grep" | "find" | "ls"))
    {
        guidelines.push(
            "Explore with one command, not one round per look. When the question is a lookup or an \
             aggregation across many files — \"find every X\", \"count Y per crate\", \"show all Z with \
             line numbers\" — plan the whole retrieval and write ONE command that emits the complete \
             answer: a `for` loop over paths, `rg -n --context 2`, an `awk`. Every message is a full \
             model round-trip, so ten small queries cost ten round-trips for what one command produces \
             at once. `rg` respects .gitignore; bound the output with `-m`, `-l`, or `| head -n`."
                .to_string(),
        );
        guidelines.push(
            "Reach for read/grep/find/ls when the next step genuinely depends on what you just saw, \
             when you need offset/limit continuation on a single file, or when one targeted lookup is \
             already the whole answer — `read paths=[...]` covers a handful of known files in one call."
                .to_string(),
        );
    }
    // 「先撒网、再下钻」：不知道答案在哪时，逐步试探会把**轮数**推高 —— 而每轮是一次完整的
    // 模型往返，轮数又是墙钟与输入 token 的共同乘数；一次覆盖全域的调用则能把候选集一次拿全。
    // 原先这里是「先窄后宽」，理由是省上下文预算；但实测里**工具执行只占墙钟 ~1%**，
    // 而多一轮要付 ~2–3s 的首 token 延迟。整文件读的代价仍由末句约束（不是取消，是降为兜底）。
    if tools.iter().any(|t| matches!(*t, "read" | "grep")) {
        guidelines.push(
            "Start wide when you don't know where the answer is: one sweeping call (a recursive \
             file listing, or a single grep for what you're after) beats several probing ones — \
             then read what you found several files at a time. Keep line ranges narrow to hold a \
             result's size down, not as the default"
                .to_string(),
        );
    }
    // 「过程叙述」：多轮工具任务中，用户看不到模型的思考，只能看到工具进出 ⇒ 完全沉默会让
    // 「改进过程迟迟没有回复」变成黑箱。但**叙述本身也是成本**：它进 `output`（真金白银生成），
    // 又留在会话里被之后**每一轮**重读 —— 实测一次 14 分钟探索写下 37 段、16,916 字符（≈4,229
    // token），其中最长一段 2,464 字符是给内部假设做的论证。那段话对用户零价值，却要被重读几十次。
    //
    // 故四条**可判定**的约束，缺一条实测就会被突破：
    //   ① 开头：**新请求的第一个工具调用之前**必须先说一句要做什么 —— 否则用户看到的是
    //      工具凭空发起，既不知道在做什么、也无从判断方向对不对。这是「聚焦需求」的唯一
    //      时点：先复述意图，再动手。
    //   ② 其余时机：只在**里程碑**（换了目标 / 得出结论 / 遇到梗阻），不是每批工具调用。
    //   ③ 长度：**≤ 120 字符**。「一句话」不可判定（一句话可以写 2,464 字符）。
    //   ④ 内容：禁止假设与论证（「可能是因为」「我猜」「先验证 A 或 B」）—— 那属于内部推理。
    //
    // ① 与 ② 不冲突：开头这一句是**最高优先级的里程碑**（回合起点），`②` 管的是它的**其余**
    // 出现时机。缺 ① 的实测形态就是「发出需求后工具直接开跑」，用户全程没有落点。
    //
    // **「连续 N 次调用必须说一句」不写在这里** —— 模型看不到自己的累计调用次数，
    // 这条已交给 harness 计数并在到阀时注入提醒（`RunLimits::narration_silence_tool_calls`）。
    // 把它写进系统提示词只会得到一条模型无法执行的要求（需自己默默数调用）。
    //
    // 只要工作型工具在场（`read` / `grep` / `find` / `ls` / `bash` / `edit` / `write` 任一）
    // 就值得提醒，否则（如纯检索类）不加，避免无谓增大固定前缀。
    // `narrate = false`（探索子 agent）时整条不渲染：它的读者是父模型，不是用户。
    if narrate
        && tools.iter().any(|t| {
            matches!(
                *t,
                "read" | "grep" | "find" | "ls" | "bash" | "edit" | "write"
            )
        })
    {
        guidelines.push(
            "Before the first tool call of a new request, state in **one short sentence** what you \
             are about to do — the user must see the intent before any tool fires. After that, \
             narrate only at milestones (target changed, conclusion reached, or blocked) — not \
             before every tool batch. Each update is **one short sentence** (hard limit 120 \
             characters) saying what you did or will do, nothing else. Never state hypotheses or \
             reasoning (\"maybe\", \"I suspect\", \"let me check whether A or B\") — those are \
             internal and cost the user a large re-read on every later round"
                .to_string(),
        );
    }
    if tools.contains(&"websearch") {
        guidelines.push(
            "Use websearch for information outside the repository (API docs, error meanings)"
                .to_string(),
        );
    }
    if tools.contains(&"webfetch") {
        // 末尾刻意写明**代价**（多一次模型调用、更慢），这是「按需使用」的唯一闸门：
        // 模型有「更努力」的倾向，措辞若只说好处，可选参数会被当成每次抓取都该带。
        guidelines.push(
            "Use webfetch to save a URL's content to a local file and get back only its head — \
             read that path (read offset=) or grep it for more, and do not refetch the same URL. \
             Pass prompt only when you need an answer rather than the page (e.g. \"how is the auth \
             header built?\"): it costs one extra model call and is slower"
                .to_string(),
        );
    }
    // 后台命令两条 guideline —— **结果自动送达，不要轮询**（`docs/arch/bg-notify/design.md`）。
    //
    // ① 长命令**后台化**，随后继续自己那部分工作：完成时由 harness 把结果送进会话；
    //    回合先结束则留表、下一回合开头补投 —— 故措辞必须交代**跨回合的送达时机**，
    //    否则模型会以为「回合结束 = 结果没了」而原地等待。
    // 「不要 poll」是**显式禁令**：本运行时没有查询后台状态的工具，模型能做的只有等通知。
    // ② 禁 `sleep` 假装在等 —— `sleep 30` 是真的睡满 30s。
    //
    // 守卫形态与既有 `websearch` / `webfetch` / `subagent` 的「在场才出现」同形，**只看
    // `bash_bg`**：两条文案都只引用它，缺了它就是指向不存在工具的悬空指令。
    // 附带效果：子表不含 bg 工具 ⇒ 子 agent 天然看不到这两条。
    if tools.contains(&"bash_bg") {
        guidelines.push(
            "Run a command that takes more than a few seconds with bash_bg, then keep working on \
             your own part — its result is delivered to you automatically when it finishes. Do \
             NOT poll for it. If the turn ends first, the result arrives at the start of your \
             next turn"
                .to_string(),
        );
        guidelines.push(
            "Never use sleep to wait for something: start it with bash_bg; you will be notified \
             when it finishes"
                .to_string(),
        );
    }
    // 异步派发：**结果自动送达，不要 poll**。只在工具在场时出现（子表没有它）。
    if tools.contains(&"subagent_async") {
        guidelines.push(
            "Use subagent_async instead of subagent when you want to dispatch several sub-tasks \
             or keep answering the user while a sub-agent works: it returns a handle immediately \
             and the result is delivered to you automatically when it finishes — do NOT poll for \
             it. Keep doing your own part after dispatching. If you finish your own part first, \
             the turn keeps running until the sub-agent reports back, so never claim it is still \
             running and never re-dispatch the same work"
                .to_string(),
        );
        // 用户要求的「回复后立即汇报并继续」：**报告是义务**，不是可选风格。
        guidelines.push(
            "When a sub-agent's result arrives, say in one line what it found and what you are \
             doing next, then continue"
                .to_string(),
        );
    }
    if tools.contains(&"subagent") {
        // 防失控 + 防重复劳动（来源：Anthropic 多 agent 博客的量化经验，见
        // `docs/arch/subagent/research.md` §2.9）。措辞要点：
        // 规模规则、任务描述四要素、依赖与写入范围、失败诊断、成本提醒。
        // 并发槽位数读 `ys-component` 的**单一真值常量**（ADR-0022 决策 14 的双轨：
        // 常量是唯一真值，`RunLimits.max_parallel_tools` 与这里都取它）。
        guidelines.push(format!(
            "At most {} subagents run in parallel in one turn; extra calls queue. Dispatch only \
             as many independent sub-tasks as you actually have.",
            ys_component::DEFAULT_MAX_PARALLEL_TOOLS
        ));
        guidelines.push(
            "Use subagent to keep big, self-contained investigations out of this context: the \
             sub-agent does NOT see this conversation, so `prompt` must be fully self-contained"
                .to_string(),
        );
        // **成批派发是机制事实，不是风格建议**：同一条消息里的多个 `subagent` 调用进同一个
        // 并行段（`is_concurrency_safe == true`，`BasicLoop` Step 9 的 `Segment::Parallel`）
        // ⇒ 真并发；而**一轮一个**会一个跑完才轮到下一个 ⇒ 串行 N 倍。
        // 实测（会话 `1792579023_125340094` turn 16）：3 个派发分在 3 轮里，
        // 串行 1,831 s；同一批只需 965 s。
        //
        // 这条**替换**了原先只讲「规模」的那句 —— 前缀成本按字节计，不新增条目。
        guidelines.push(
            "Scale the dispatch: one subagent is enough for a simple lookup. For genuinely \
             independent sub-tasks, send ALL the `subagent` calls in ONE message — a single \
             batch runs concurrently, whereas one call per round runs them one after another \
             (N times slower)"
                .to_string(),
        );
        guidelines.push(
            "Write each `prompt` with four parts: goal, expected output format, sources/tools to \
             use, and explicit boundaries — vague prompts make the sub-agents repeat each other's work"
                .to_string(),
        );
        guidelines.push(
            "Before dispatching multiple subagents, check their dependencies and write scopes. If \
             a task consumes another task's output or verifies its changes, wait for the \
             prerequisite to finish. Run tasks in parallel only when their outputs are \
             independent and their write scopes do not overlap; serialize them when the scope is \
             unclear"
                .to_string(),
        );
        guidelines.push(
            "If a subagent returns an error or is cancelled, read its diagnostic first, then \
             either narrow the task and retry or report the failure to the user"
                .to_string(),
        );
        guidelines.push(
            "A subagent is not free: it is another inference call plus another system prompt — \
             use it to save context, not to look busy"
                .to_string(),
        );
        // 派发探索只在「多个独立、每个都要多轮」的问题上划算：子 agent 每问多付
        // 约 25–30 s 固定开销（冷启动 + 独立系统提示），一两个小查找父 agent 自己搜更快。
        // 成批派发才是收益点 —— 同批调用进 `Segment::Parallel`。
        guidelines.push(
            "Delegate exploration only when you have several independent questions that each need \
             many rounds: send them in ONE message as `subagent` with `mode=\"explore\"` so they run \
             in parallel, then trust their `path:line` findings. For one or two small lookups, search \
             directly — an agent's cold start costs more than the lookup itself."
                .to_string(),
        );
    }

    // 任务边界：只做用户要求的事。必要步骤（读取/搜索、对改动代码跑检查、修复自身改动造成的
    // 破坏）免问；其余（无关重构、新文件/文档、依赖变更、大范围格式化、额外测试、超出要求的
    // commit/push）先一行提议并等待。单发模式不能问时只做被要求的部分并列出未做项。
    // 与具体工具无关，故无条件渲染 —— 只有 `bash` 时同样在场。
    guidelines.push(
        "Do exactly what the user asked; do not add work they did not request. \
         Necessary steps are allowed without asking: reading/searching to understand the task, \
         running checks on code you changed, and fixing breakage your own change caused. \
         Everything else must be proposed first — unrelated refactors, new files or docs, \
         dependency changes, broad reformatting, extra test suites, or committing/pushing \
         beyond what was asked. State the proposal in one line and wait for the user's answer. \
         If you cannot ask (single-shot run), do only what was asked and list the extra work as not done."
            .to_string(),
    );

    guidelines.push("Be concise in your responses".to_string());
    guidelines.push("Show file paths clearly when working with files".to_string());

    guidelines
        .iter()
        .map(|g| format!("- {g}"))
        .collect::<Vec<_>>()
        .join("\n")
}

struct ContextFile {
    path: std::path::PathBuf,
    content: String,
}

/// 加载项目上下文文件（`<project_context>` 段）。
///
/// # 搜索名单：`AGENTS.override.md` / `AGENTS.md` / `YUSHAN.md`
///
/// 从 `cwd` **逐级向上**遍历，每一级按上述顺序各试一次；命中的**全收**（不是首个命中即停），
/// 最后 `reverse` 成「祖先目录从远到近」。另有一份全局上下文 `~/.yushan/AGENTS.md`，
/// 排在所有逐级命中之前。
fn load_project_context(cwd: &Path) -> Vec<ContextFile> {
    let mut files = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // 全局上下文
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        try_load(
            &std::path::PathBuf::from(home).join(".yushan/AGENTS.md"),
            &mut files,
            &mut seen,
        );
    }

    // 从 cwd 逐级向父目录遍历。**`CLAUDE.md` 刻意不在名单里**（见函数文档）。
    let mut current = cwd.to_path_buf();
    loop {
        for name in &["AGENTS.override.md", "AGENTS.md", "YUSHAN.md"] {
            try_load(&current.join(name), &mut files, &mut seen);
        }
        let parent = current.parent();
        if parent.is_none() || parent == Some(current.as_path()) {
            break;
        }
        current = parent.unwrap().to_path_buf();
    }

    files.reverse(); // 祖先目录从远到近
    files
}

fn try_load(
    path: &std::path::Path,
    files: &mut Vec<ContextFile>,
    seen: &mut std::collections::HashSet<std::path::PathBuf>,
) {
    if path.exists()
        && !seen.contains(path)
        && let Ok(content) = std::fs::read_to_string(path)
    {
        files.push(ContextFile {
            path: path.to_path_buf(),
            content,
        });
        seen.insert(path.to_path_buf());
    }
}

fn find_custom_prompt(cwd: &Path) -> Option<String> {
    // 检查 .yushan/SYSTEM.md
    let project_path = cwd.join(".yushan/SYSTEM.md");
    if project_path.exists() {
        return std::fs::read_to_string(project_path).ok();
    }
    // 检查 ~/.yushan/SYSTEM.md
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let global_path = std::path::PathBuf::from(home).join(".yushan/SYSTEM.md");
        if global_path.exists() {
            return std::fs::read_to_string(global_path).ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::{EnvRestore, env_lock};
    use crate::toolset::EXPECTED_ORDER;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 造一个只关心 name 的 spec（一句话描述由 `prompt.rs` 按 name 查表提供，
    /// `description` 字段本函数不读 —— 断言里体现为「改 description 不影响输出」）。
    fn spec(name: &str) -> ToolSpec {
        ToolSpec::new(name, "（本函数不读这个字段）", serde_json::json!({}))
    }

    /// **空** catalogue（= 没有 skill，或预算把整表收起）—— 严格前缀不变量的左端。
    fn no_skills() -> crate::skills::Catalogue {
        crate::skills::catalogue(Vec::new(), crate::skills::Budget::default())
    }

    /// 含一个 skill 的 catalogue（合成条目，不落盘）。
    fn one_skill(command: &str, description: &str) -> crate::skills::Catalogue {
        use crate::skills::{Scope, SkillEntry};
        crate::skills::catalogue(
            vec![SkillEntry {
                command: command.to_string(),
                label: command.to_string(),
                description: description.to_string(),
                path: PathBuf::from(format!("/skills/{command}/SKILL.md")),
                scope: Scope::User,
                model_visible: true,
                user_invocable: true,
            }],
            crate::skills::Budget::default(),
        )
    }

    /// 进程内唯一临时目录：**原子序号 + `process::id()`**（项目硬约定）。
    ///
    /// `SystemTime::as_nanos()` 在 macOS 只到微秒级，并行测试会撞名互删（血的教训）。
    fn unique_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("yushan_prompt_{tag}_{}_{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// **确定性** `WebConfig`（测试不得用 `from_env()`：结果随机器 env 漂移）。
    ///
    /// 本用例只关心工具**名字**，web 侧配不配无关 —— 但装配接缝要求传一个值，
    /// 且 `offline` 不建任何真实用户目录（`Temp` 档）。
    fn offline_web() -> ys_tools_web::WebConfig {
        ys_tools_web::WebConfig::offline(ys_tools_web::FetchedDir::Temp(unique_dir("web")))
    }

    /// 抽出 `Available tools:` 段的行（形状 `- {name}: {snippet}`），保持出现顺序。
    ///
    /// 段结束标记用后面的空行 —— 这是 `build_system_prompt` 模板里的固定形状；
    /// 模板若改形状（丢掉空行/改标题），此处 expect 会立刻炸而不是静默返回空。
    fn listed_tool_lines(prompt: &str) -> Vec<&str> {
        let marker = "Available tools:\n";
        let start = prompt
            .find(marker)
            .expect("默认 prompt 应含 `Available tools:` 段")
            + marker.len();
        let rest = &prompt[start..];
        let end = rest
            .find("\n\n")
            .expect("工具清单段应以空行结束（后接 Guidelines）");
        rest[..end].lines().collect()
    }

    /// 清单里出现的工具名序列（从 `- {name}: …` 行里取 name）。
    fn listed_tool_names(prompt: &str) -> Vec<&str> {
        listed_tool_lines(prompt)
            .into_iter()
            .map(|line| {
                line.strip_prefix("- ")
                    .and_then(|body| body.split(':').next())
                    .expect("清单行形状应为 `- {name}: {snippet}`")
            })
            .collect()
    }

    /// 把 `HOME`/`USERPROFILE` 指向一个**不存在**的目录，让 `~/.yushan/SYSTEM.md`
    /// （全局自定义 prompt）绝不会命中 —— 否则开发机上存在该文件时，下面的清单
    /// 断言会被「最早返回短路」整体跳过而变红。
    ///
    /// 调用方必须先持有 [`env_lock`]。
    fn no_global_prompt() -> EnvRestore {
        let env = EnvRestore::capture(&["HOME", "USERPROFILE"]);
        // SAFETY: 调用方持有 env_lock，独占这两个 key 的修改与恢复。
        unsafe {
            std::env::set_var("HOME", "/nonexistent-yushan-test-home");
            std::env::remove_var("USERPROFILE");
        }
        env
    }

    #[test]
    fn test_default_prompt_contains_tools() {
        // build_system_prompt 会读 HOME/USERPROFILE 查找全局 prompt，
        // 与改写这两个 key 的测试共享同一把锁，避免读到中间态。
        let _guard = env_lock();
        let _env = no_global_prompt();

        let specs = vec![spec("read"), spec("write"), spec("edit"), spec("bash")];
        let prompt = build_system_prompt(Path::new("/tmp"), &specs, &no_skills());

        // 清单**逐字**按传入顺序渲染（保序由装配端保证，此处只锁定渲染形状）。
        let expected = "Available tools:\n\
                        - read: Read file contents\n\
                        - write: Create or overwrite files\n\
                        - edit: Make precise file edits with exact text replacement\n\
                        - bash: Execute bash commands";
        assert!(
            prompt.contains(expected),
            "清单应逐字等于传入 specs 的顺序；prompt = {prompt}"
        );
        assert!(prompt.contains("coding agent"));
    }

    /// **清单派生（N8）**：清单逐条等于传入 specs 的 name 序列 —— 不排序、
    /// 不查内部名录、不去重。
    ///
    /// 用**两份相反顺序**各跑一次：只跑正向那份的话，「实现偷偷按固定名录排序」
    /// 会与 `EXPECTED_ORDER` 恰好同形而蒙混过关；逆序那份才真正钉住「派生」。
    ///
    /// 顺序真值来自 [`toolset::EXPECTED_ORDER`]（装配端的唯一真值），此处**不另抄**。
    ///
    /// 变异 1：`build_system_prompt` 里给 `names` 加 `.rev()` → 正向断言变红。
    /// 变异 2：实现改成按内部固定名录排序（无视传入顺序）→ 逆序断言变红。
    #[test]
    fn tool_list_is_derived_from_specs_order_verbatim() {
        let _guard = env_lock();
        let _env = no_global_prompt();

        let reversed: Vec<&str> = EXPECTED_ORDER.iter().rev().copied().collect();
        for order in [EXPECTED_ORDER.to_vec(), reversed] {
            let specs: Vec<ToolSpec> = order.iter().map(|name| spec(name)).collect();
            let prompt = build_system_prompt(Path::new("/tmp"), &specs, &no_skills());

            assert_eq!(
                listed_tool_names(&prompt),
                order,
                "清单顺序必须逐条等于传入 specs 序列；prompt = {prompt}"
            );
            // 内容：每行逐字为 `- {name}: {snippet}`。
            let expected_lines: Vec<String> = order
                .iter()
                .map(|name| format!("- {name}: {}", tool_snippet(name)))
                .collect();
            let actual_lines: Vec<String> = listed_tool_lines(&prompt)
                .into_iter()
                .map(str::to_string)
                .collect();
            assert_eq!(
                actual_lines, expected_lines,
                "清单每行形状/文案应与 name 查表一致"
            );
        }
    }

    /// **漂移守卫（N8 核心）**：**真实 toolset** 的每个工具都必须有专属（非兜底）
    /// snippet。
    ///
    /// 工具表（`toolset::build_tools`）与 snippet 表（`tool_snippet`）是两份手工维护
    /// 的清单：只加工具、忘加 snippet 时，提示词会静默退化为 `- newtool: Custom tool`
    /// —— 编译通过、其余测试全绿，只有用户读到废话。本用例以**装配端的真实产物**
    /// 为输入、以 snippet 表为被测，逐名断言。
    ///
    /// 变异 1：删掉 `tool_snippet` 的 `"grep"` 分支 → 变红（`grep` 落兜底）。
    /// 变异 2：`toolset::build_tools` 新增一个工具而 snippet 未跟上 → 同样变红。
    #[test]
    fn every_real_toolset_tool_has_non_fallback_snippet() {
        let _guard = env_lock();
        let _env = no_global_prompt();
        let dir = unique_dir("drift");

        let (tools, _hint) = crate::toolset::build_tools(&dir, offline_web());
        let names: Vec<String> = tools.iter().map(|t| t.spec().name).collect();

        // 先确认取到的是真实工具表（空表会让下面的循环平凡通过）。
        let actual: Vec<&str> = names.iter().map(String::as_str).collect();
        assert_eq!(actual, EXPECTED_ORDER, "应取到装配端的真实 9 元工具表");

        for name in &names {
            let snippet = tool_snippet(name);
            assert_ne!(
                snippet, "Custom tool",
                "工具 `{name}` 没有专属 snippet，提示词会退化为兜底文案 —— \
                 加工具时请同步更新 `tool_snippet`"
            );
            assert!(!snippet.is_empty(), "工具 `{name}` 的 snippet 不得为空");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `grep` / `find` / `ls` / `websearch` / `webfetch` 的 snippet **逐字**钉死
    /// （T11 review 点名的无守卫项 + T13 新增的 web 两条）。
    ///
    /// 非兜底断言（上一用例）只能证明「有专属文案」，证不了「文案说对了」；
    /// 这几条是模型真正读到的检索 / 列目录 / 联网指引，故用字面量锁文本。
    ///
    /// 文案里的关键语义（websearch 的「固定顺序试后端、首答即停」；webfetch 的
    /// 「落盘 + 只回头部 + 用 `read offset=` 续读」）都是**模型行为的输入**，
    /// 改文案等于改行为 —— 必须过一次显式评审，故此守卫用逐字断言。
    /// （可选 `prompt` 取摘要的线索同理，见 `web_guidelines_*` 用例。）
    ///
    /// 变异：把任一分支的文案改成别的（哪怕仍非兜底）→ 本用例变红。
    #[test]
    fn search_list_and_web_snippets_are_verbatim() {
        assert_eq!(
            tool_snippet("grep"),
            "Search file contents for a pattern (respects .gitignore when ripgrep is available)"
        );
        assert_eq!(tool_snippet("find"), "Search for files by glob pattern");
        assert_eq!(tool_snippet("ls"), "List directory contents");
        assert_eq!(
            tool_snippet("websearch"),
            "Search the web (tries the configured backends in a fixed order, stops at the first one that answers)"
        );
        assert_eq!(
            tool_snippet("webfetch"),
            "Fetch a URL as Markdown saved to a file, returning the head plus the full path (continue with read offset= or grep, or pass prompt for a summary)"
        );
        // 兜底分支本身也要在（外部工具靠它列出而不报错）。
        assert_eq!(tool_snippet("__no_such_tool__"), "Custom tool");
    }

    /// web 两条 guideline 在工具**在场**时出现、**缺席**时不出现（照 `grep`/`find` 的
    /// 「有工具才提示」风格）。
    ///
    /// 反向断言（缺席时不得出现）是关键：少了它，一个「无条件把所有 guideline 都塞进去」
    /// 的实现也能过 —— 而那会让模型看到用不了的工具建议。
    ///
    /// 变异：把两条 guideline 的 `if tools.contains(&"websearch")` / `webfetch` 守卫
    /// 删掉（无条件加）→ 反向断言变红。
    #[test]
    fn web_guidelines_appear_only_when_tools_present() {
        let _guard = env_lock();
        let _env = no_global_prompt();

        let with_web = build_system_prompt(
            Path::new("/tmp"),
            &[spec("read"), spec("websearch"), spec("webfetch")],
            &no_skills(),
        );
        assert!(
            with_web.contains("- Use websearch for information outside the repository"),
            "有 websearch 时应给「查仓库外信息」的指引；prompt = {with_web}"
        );
        assert!(
            with_web.contains("- Use webfetch to save a URL's content to a local file"),
            "有 webfetch 时应给「落盘 + 读回路径」的指引；prompt = {with_web}"
        );
        assert!(
            with_web.contains("do not refetch the same URL"),
            "webfetch 指引应含「别重复抓同一 URL」；prompt = {with_web}"
        );
        // 可选 `prompt` 的**代价**必须告知模型：这是「按需用」而非「每次都用」的唯一闸门。
        // 只断言「代价已告知」，不断言具体措辞 —— 措辞可调，代价不可省。
        assert!(
            with_web.contains("costs one extra model call"),
            "webfetch 指引应告知带 prompt 的代价（多一次模型调用）；prompt = {with_web}"
        );

        let without_web = build_system_prompt(Path::new("/tmp"), &[spec("read")], &no_skills());
        assert!(
            !without_web.contains("Use websearch"),
            "无 websearch 时不得出现其指引；prompt = {without_web}"
        );
        assert!(
            !without_web.contains("Use webfetch"),
            "无 webfetch 时不得出现其指引；prompt = {without_web}"
        );
    }

    /// 探索指引要求**一条命令产出完整答案**（强制综合），并把旧的「用结构化工具代替 bash」
    /// 四条彻底移除。
    ///
    /// 承重语义：`ONE command` 要求命令自己完成筛选与汇总、`rg -n --context` 给出聚合手段、
    /// `Every message is a full model round-trip` 说明代价模型、`.gitignore` 语义与有界输出
    /// 的手段 —— 这些都是模型写命令时的直接输入。
    ///
    /// 反向断言同样关键：`SAME message` 会被理解成「多跑几条命令」，实测总调用数 +33%、
    /// 轮数 +35%（广度枚举而非减少往返），不得再出现；`back-to-back inside one round` 是同一
    /// 措辞的伴生短语，同样不得出现。旧四条把可聚合的检索拆成逐轮结构化调用，也不得再出现。
    ///
    /// 变异：把 `ONE command` 改回 `SAME message`（或补上 `back-to-back inside one round`）
    /// ⇒ 对应反向断言变红；
    /// 删掉 `Explore with one command` ⇒ 正向断言变红；
    /// 删掉 `rg -n --context` 或 `.gitignore` ⇒ 对应断言变红；
    /// 恢复 `Use read to examine files instead of cat or sed`（或 grep 那条）⇒ 反向断言变红。
    #[test]
    fn explore_guideline_prefers_one_aggregating_command() {
        let _guard = env_lock();
        let _env = no_global_prompt();

        let prompt = build_system_prompt(
            Path::new("/tmp"),
            &[spec("read"), spec("grep"), spec("bash")],
            &no_skills(),
        );

        for fragment in [
            "Explore with one command",
            "ONE command",
            "rg",
            ".gitignore",
        ] {
            assert!(
                prompt.contains(fragment),
                "探索指引须含 `{fragment}`；prompt = {prompt}"
            );
        }
        assert!(
            !prompt.contains("SAME message"),
            "`SAME message` 会诱导广度枚举（实测调用 +33%、轮数 +35%），必须移除；prompt = {prompt}"
        );
        assert!(
            !prompt.contains("back-to-back inside one round"),
            "`back-to-back inside one round` 属同一措辞的伴生短语，必须移除；prompt = {prompt}"
        );
        assert!(
            !prompt.contains("Use read to examine files instead of cat or sed"),
            "旧的「用 read 代替 cat/sed」必须移除；prompt = {prompt}"
        );
        assert!(
            !prompt.contains("instead of running grep through bash"),
            "旧的「用 grep 代替 bash grep」必须移除；prompt = {prompt}"
        );
    }

    /// 探索两条指引**只在只读工具在场时**出现。
    ///
    /// 正向：`read` 在场即渲染，且给出结构化工具的胜出场景（依赖下一步、offset/limit 续读、
    /// 单次定向查询）。
    ///
    /// 反向断言（只有 `bash` 时不得出现）是关键：文案引用 `rg` 与 `read paths=[...]`，
    /// 模型在没有这些工具时照做只会得到不存在的工具名。
    ///
    /// 变异：把守卫从 `any(read|grep|find|ls)` 改成无条件 push ⇒ 反向断言变红。
    #[test]
    fn explore_guideline_appears_only_with_read_only_tools() {
        let _guard = env_lock();
        let _env = no_global_prompt();

        let with_read = build_system_prompt(Path::new("/tmp"), &[spec("read")], &no_skills());
        assert!(
            with_read.contains("Explore with one command"),
            "只读工具在场时应给聚合探索指引；prompt = {with_read}"
        );
        assert!(
            with_read.contains("Reach for read/grep/find/ls when the next step genuinely depends"),
            "须正向陈述结构化工具的胜出场景；prompt = {with_read}"
        );

        let bash_only = build_system_prompt(Path::new("/tmp"), &[spec("bash")], &no_skills());
        assert!(
            !bash_only.contains("Explore with one command"),
            "只有 bash 时不得出现聚合探索指引；prompt = {bash_only}"
        );
        assert!(
            !bash_only.contains("Reach for read/grep/find/ls"),
            "只有 bash 时不得出现结构化工具胜出场景；prompt = {bash_only}"
        );
    }

    /// 写入指引：仍走 `edit` / `write`，禁脚本化变更；拆多次调用是允许的。
    ///
    /// 承重语义：`never with scripts`（脚本化写入绕过 `oldText` 精确匹配校验）与
    /// `Splitting writes across several calls is fine`（拆调用本身没有代价）。
    ///
    /// 反向断言：`edit` / `write` 都不在场时不得出现 —— 文案只引用这两个工具。
    ///
    /// 变异：删掉 `never with scripts` ⇒ 第一条变红；删掉 `Splitting writes across several
    /// calls is fine` ⇒ 第二条变红；把守卫改成无条件 push ⇒ 反向断言变红。
    #[test]
    fn write_guideline_keeps_edits_structured() {
        let _guard = env_lock();
        let _env = no_global_prompt();

        let with_edit = build_system_prompt(Path::new("/tmp"), &[spec("edit")], &no_skills());
        assert!(
            with_edit.contains("never with scripts"),
            "edit 在场时须禁止脚本化写入；prompt = {with_edit}"
        );
        assert!(
            with_edit.contains("Splitting writes across several calls is fine"),
            "须说明拆多次写入没有代价；prompt = {with_edit}"
        );

        let no_writers = build_system_prompt(Path::new("/tmp"), &[spec("read")], &no_skills());
        assert!(
            !no_writers.contains("never with scripts"),
            "edit/write 都不在场时不得出现写入指引；prompt = {no_writers}"
        );
    }

    /// A7 任务边界 guideline：与具体工具无关，`bash` 在场时必须出现；
    /// 含「必要步骤免问」与「不能问就只做被要求的部分并列出未做项」两句。
    ///
    /// 变异：① 把它挪进 `tools.contains(...)` 分支 ⇒ bash-only 断言变红；
    /// ② 删掉 `Necessary steps are allowed without asking` ⇒ 必要步骤断言变红；
    /// ③ 删掉 `do only what was asked and list the extra work as not done` ⇒ 单发模式断言变红。
    #[test]
    fn task_boundary_guideline_appears_with_bash_and_has_two_constraints() {
        let _guard = env_lock();
        let _env = no_global_prompt();

        let with_bash = build_system_prompt(Path::new("/tmp"), &[spec("bash")], &no_skills());
        assert!(
            with_bash.contains(
                "- Do exactly what the user asked; do not add work they did not request."
            ),
            "bash 在场时必须出现任务边界 guideline；prompt = {with_bash}"
        );
        assert!(
            with_bash.contains("Necessary steps are allowed without asking"),
            "须写明「必要步骤免问」；prompt = {with_bash}"
        );
        assert!(
            with_bash.contains("do only what was asked and list the extra work as not done"),
            "须写明「不能问就只做被要求的部分并列出未做项」；prompt = {with_bash}"
        );

        // 与工具无关：空工具表也应出现（它不是工具建议，而是任务边界）。
        let no_tools = build_system_prompt(Path::new("/tmp"), &[], &no_skills());
        assert!(
            no_tools.contains("Do exactly what the user asked"),
            "任务边界 guideline 与工具无关，空工具表也应出现；prompt = {no_tools}"
        );
    }

    /// 「先撒网」这条 guideline **只在 `read` / `grep` 在场时**出现。
    ///
    /// 它替换了原先的「先窄后宽」：窄探索省上下文，但把**轮数**推高，而轮数才是墙钟与
    /// 输入 token 的乘数（实测工具执行只占墙钟 ~1%，而多一轮要付 ~2–3s 首 token 延迟）。
    /// 窄区间**没有取消**，只是从默认降为兜底（末句）。
    ///
    /// 变异：① 把守卫从 `any(read|grep)` 改成无条件 push ⇒ 第三条断言变红；
    /// ② 去掉末句的 "not as the default" ⇒ 第二条断言变红（窄区间又变回默认）。
    #[test]
    fn wide_first_guideline_appears_only_with_read_or_grep() {
        let _guard = env_lock();
        let _env = no_global_prompt();

        let with_tools = build_system_prompt(
            Path::new("/tmp"),
            &[spec("read"), spec("grep"), spec("bash")],
            &no_skills(),
        );
        assert!(
            with_tools.contains("- Start wide when you don't know where the answer is"),
            "有 read/grep 时应给「先撒网」指引；prompt = {with_tools}"
        );
        assert!(
            with_tools.contains("not as the default"),
            "窄区间须降为兜底而非默认 —— 这是本次改动的实质；prompt = {with_tools}"
        );
        assert!(
            with_tools.contains("then read what you found several files at a time"),
            "指引须给出下钻动作（一次读多个已知文件），而不只是劝阻；prompt = {with_tools}"
        );

        let bash_only = build_system_prompt(Path::new("/tmp"), &[spec("bash")], &no_skills());
        assert!(
            !bash_only.contains("Start wide when you don't know"),
            "只有 bash 时不得出现该指引；prompt = {bash_only}"
        );
    }

    /// 「过程叙述」guideline 的**新契约**：新请求的首个工具调用前先说一句、
    /// 其余仅在里程碑说话、限 120 字符、禁假设推理。
    ///
    /// 历史教训（为什么这几条要分开钉）：旧措辞是「Before every batch … one concise sentence」，
    /// 两条都不可判定 —— `every batch` 被照做成 37 次，`one sentence` 被写成 2,464 字符的论证。
    /// ⇒ 现在每条约束都有**能不能被引用原文证明**的断言，而不是靠「测试绿了」。
    ///
    /// 反向断言（空工具表 / 纯检索工具表不得出现）仍然关键：少了它，「无条件 push」的实现
    /// 也能过 —— 而那会让固定前缀无谓变大。
    ///
    /// 变异：① 把守卫改成无条件 push ⇒ 最后两条变红；② 把 `bash` 从匹配集合删掉 ⇒ bash 那条变红；
    /// ③ 删掉 `120` ⇒ 长度断言变红；④ 改成「before every batch」⇒ 里程碑断言变红；
    /// ⑤ 删掉开头那一句 ⇒ 「首个工具调用前说一句」断言变红。
    #[test]
    fn narration_guideline_is_milestone_scoped_and_bounded() {
        let _guard = env_lock();
        let _env = no_global_prompt();

        // 定位这句 guideline 的**起头**（后续断言都在它上面做）。
        let opener = "- Before the first tool call of a new request";
        let marker = "narrate only at milestones";

        let with_read = build_system_prompt(Path::new("/tmp"), &[spec("read")], &no_skills());
        assert!(
            with_read.contains(opener),
            "含 read 时应给「过程叙述」指引；prompt = {with_read}"
        );
        // ① 开头：新请求的首个工具调用前必须先声明意图。
        //
        // 这是「发出需求后工具直接开跑」的那个故障的守卫：缺它时模型可以一声不吭
        // 连续调工具，用户全程没有落点、也无从判断方向对不对。
        assert!(
            with_read.contains("Before the first tool call"),
            "必须要求首个工具调用前先说明意图；prompt = {with_read}"
        );
        assert!(
            with_read.contains("the user must see the intent before any tool fires"),
            "必须写明该句的理由（意图先于工具）；prompt = {with_read}"
        );
        // ② 其余时机：里程碑驱动，**不是**每批一次。
        //
        // 注意：新文案里带着 “not before every tool batch” 这半句（它是在**否定**旧行为），
        // 所以负向断言只能针对**旧措辞的原文**（`Before every batch of tool calls`），
        // 不能针对 `before every tool batch` 这种子串 —— 那会把新文案自己也判红。
        assert!(
            with_read.contains(marker),
            "必须声明「其余仅在里程碑」；prompt = {with_read}"
        );
        // ② 长度：可判定的数字上限（旧措辞的「一句话」不可判定）。
        assert!(
            with_read.contains("120 characters"),
            "必须给出可判定的字符上限；prompt = {with_read}"
        );
        // ②b 形态：「一句话」—— 模型按句组织语言，按字符数得先数。两条要同时在场。
        assert!(
            with_read.contains("one short sentence"),
            "必须要求「一句话」而非仅字符数；prompt = {with_read}"
        );
        // ③ 内容：禁止假设与论证 —— 这是实测里最贵的那部分。
        assert!(
            with_read.contains("Never state hypotheses or reasoning"),
            "必须禁止假设与推理外化；prompt = {with_read}"
        );
        // ④ 静默阀值**不属于系统提示** —— 模型数不准自己的调用次数，该由 harness 计数
        //    并在到阀时注入提醒（`RunLimits::narration_silence_tool_calls`）。
        assert!(
            !with_read.contains("12 tool calls"),
            "静默阀值不得写进系统提示（模型无法自己计数）；prompt = {with_read}"
        );
        // 旧措辞必须不再出现（否则新旧并存，模型会照旧的那句做）。
        assert!(
            !with_read.contains("Before every batch of tool calls"),
            "旧的「每批都写」措辞必须移除；prompt = {with_read}"
        );
        assert!(
            !with_read.contains("Keep updates to one sentence"),
            "不可判定的「一句话」措辞必须移除；prompt = {with_read}"
        );

        let with_bash = build_system_prompt(Path::new("/tmp"), &[spec("bash")], &no_skills());
        assert!(
            with_bash.contains(opener),
            "含 bash 时应给「过程叙述」指引；prompt = {with_bash}"
        );

        let no_tools = build_system_prompt(Path::new("/tmp"), &[], &no_skills());
        // 反向断言用 guideline 的**起头**，不用某个旧短语 —— 后者会随文案修改
        // 静默变成恒真（`!contains(旧短语)` 在新文案里自然成立，根本验证不到门控）。
        assert!(
            !no_tools.contains(opener),
            "空工具表时不得出现过程叙述指引；prompt = {no_tools}"
        );
        assert!(
            !no_tools.contains(marker),
            "空工具表时不得出现里程碑要求；prompt = {no_tools}"
        );

        let web_only = build_system_prompt(Path::new("/tmp"), &[spec("websearch")], &no_skills());
        assert!(
            !web_only.contains(marker),
            "纯检索工具表不得出现工作型进展要求；prompt = {web_only}"
        );
    }

    #[test]
    fn test_custom_prompt_takes_precedence() {
        // `find_custom_prompt` 读 HOME（全局回退路径），须与改写 env 的测试串行。
        let _guard = env_lock();
        let _env = no_global_prompt();
        // 临时目录名含进程内原子序号 + pid（并行安全）。
        let tmp = unique_dir("custom");
        let system_dir = tmp.join(".yushan");
        std::fs::create_dir_all(&system_dir).unwrap();
        std::fs::write(system_dir.join("SYSTEM.md"), "Custom system prompt").unwrap();

        let specs = vec![spec("read"), spec("bash")];
        let prompt = build_system_prompt(&tmp, &specs, &no_skills());
        assert_eq!(prompt, "Custom system prompt");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ─────────────────────────── skill 目录段（M2） ───────────────────────────

    /// **严格前缀不变量**（本设计可测的核心性质）：空 catalogue 的输出必须是任何
    /// 非空 catalogue 输出的**逐字节前缀** —— 它是「同一台机器上先后两次启动，
    /// 提示前缀稳定」的形式化（前缀稳定 ⇒ 二次启动的 KV cache 命中大部分）。
    ///
    /// 变异：把 skill 段插到 `Guidelines:` **之前**（或任何非末尾位置）⇒ 非空输出
    /// 在中段多出内容，`starts_with` 立刻变红。
    #[test]
    fn skills_section_is_appended_at_the_tail_keeping_strict_prefix() {
        let _guard = env_lock();
        let _env = no_global_prompt();
        let dir = unique_dir("skills_prefix");

        let specs = vec![spec("read"), spec("bash")];
        let bare = build_system_prompt(&dir, &specs, &no_skills());
        let with_skills =
            build_system_prompt(&dir, &specs, &one_skill("git-status", "查看仓库状态"));

        assert!(
            with_skills.starts_with(&bare),
            "空 catalogue 的输出必须是含 skill 输出的逐字节前缀；\nbare = {bare}\nwith = {with_skills}"
        );
        assert_ne!(bare, with_skills, "含 skill 时必须真的多出内容");
        // 段确实在**最末尾**：提取出来的后缀就是整段。
        let tail = &with_skills[bare.len()..];
        assert!(
            tail.starts_with("\n\n<available_skills>") && tail.ends_with("</available_skills>"),
            "追加的段落形状不对：{tail}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 段的**内容契约**（设计 §CC 兼容性 偏离 3 / R7）：三句承重前言逐项在位。
    ///
    /// 变异：删掉「相对路径按 skill 目录解析」那句 ⇒ 下文断言变红（实测多条 skill
    /// 正文按相对路径交叉引用兄弟 skill，缺了这句必然找不到）。
    #[test]
    fn skills_section_content_contract() {
        let _guard = env_lock();
        let _env = no_global_prompt();
        let dir = unique_dir("skills_content");

        let prompt = build_system_prompt(
            &dir,
            &[spec("read")],
            &one_skill("arch-design", "多方案架构设计"),
        );

        // 条目逐字来自 `Catalogue.lines`（含 location —— 零工具路线的唯一线索）。
        assert!(
            prompt
                .contains("- arch-design: 多方案架构设计 (location: /skills/arch-design/SKILL.md)"),
            "skill 行应逐字来自 catalogue.lines；prompt = {prompt}"
        );
        assert!(prompt.contains("with the read tool"), "须指出用 read 打开");
        assert!(
            prompt.contains("relative to that skill's own directory"),
            "须说明正文里的相对路径按 skill 自身目录解析"
        );
        assert!(
            prompt.contains("no subagents") && prompt.contains("no plan mode"),
            "须声明能力边界（正文里的 Claude 专有工具名在本 runtime 失效）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **门控**：无 `read` 工具（skill 正文加载不了）或目录为空时**一字不加**。
    ///
    /// 后半条尤其承重：空目录若留下一个空行，前缀不变量的「逐字节」也就破了。
    ///
    /// 变异：删掉 `!catalogue.lines.is_empty()` ⇒ 后半条断言变红（多出 `\n\n`）。
    #[test]
    fn skills_section_is_gated_on_read_tool_and_non_empty_lines() {
        let _guard = env_lock();
        let _env = no_global_prompt();
        let dir = unique_dir("skills_gate");

        // ① 没有 `read` ⇒ 即便有目录也不追加。
        let without_read =
            build_system_prompt(&dir, &[spec("bash")], &one_skill("git-status", "状态"));
        assert!(
            !without_read.contains("<available_skills>"),
            "无 read 工具时不该注入 skill 目录；prompt = {without_read}"
        );

        // ② 有 `read` 但目录为空 ⇒ 输出与「完全不加 skill 段」逐字相同（无多余空行）。
        let empty = build_system_prompt(&dir, &[spec("read")], &no_skills());
        assert!(
            empty.starts_with(&format!("Current working directory: {}", dir.display())),
            "cwd 段须在整条提示最前（实测：见 prompt.rs::cwd_section）；prompt = {empty:?}"
        );
        assert!(
            !empty.ends_with("\n\n"),
            "空目录时不得留下空行或空段；prompt = {empty:?}"
        );
        assert!(!empty.contains("available_skills"), "prompt = {empty:?}");

        // ③ 有 `read` 且有目录 ⇒ 追加。
        let with = build_system_prompt(&dir, &[spec("read")], &one_skill("git-status", "状态"));
        assert!(with.contains("<available_skills>"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **搜索名单是封闭的**：名单里的名字会加载，名单外的不会 —— 无论叫什么。
    ///
    /// 断言按 basename 判定（而非文件数或内容计数），故不依赖运行环境：祖先目录或全局
    /// 上下文若另有文件，只会多出名字，不影响这里的 `contains` / `!contains`。
    ///
    /// 变异 1：往搜索列表里多加一个名字 → 对应的 `!contains` 断言变红。
    /// 变异 2：从搜索列表里删掉 `"YUSHAN.md"` → `contains` 断言变红。
    #[test]
    fn project_context_search_list_is_closed() {
        let _guard = env_lock();
        let _env = no_global_prompt();
        let dir = unique_dir("ctx_list");

        // 名单内的三个之两个 + 两个名单外的高频上下文文件名。
        for name in ["AGENTS.md", "YUSHAN.md", "CLAUDE.md", "README.md"] {
            std::fs::write(dir.join(name), format!("{name}-content")).unwrap();
        }

        let names: Vec<String> = load_project_context(&dir)
            .iter()
            .filter_map(|f| f.path.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();

        assert!(names.contains(&"AGENTS.md".to_string()), "{names:?}");
        assert!(names.contains(&"YUSHAN.md".to_string()), "{names:?}");
        for outside in ["CLAUDE.md", "README.md"] {
            assert!(
                !names.contains(&outside.to_string()),
                "名单外的名字不得加载：{outside} ∈ {names:?}"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `has_custom_prompt` 的真/假两态（供 `main` 的启动期提示判定）。
    #[test]
    fn has_custom_prompt_reports_both_states() {
        let _guard = env_lock();
        let _env = no_global_prompt();
        let dir = unique_dir("has_custom");

        assert!(!has_custom_prompt(&dir), "无 SYSTEM.md ⇒ false");

        let system_dir = dir.join(".yushan");
        std::fs::create_dir_all(&system_dir).unwrap();
        std::fs::write(system_dir.join("SYSTEM.md"), "Custom").unwrap();
        assert!(has_custom_prompt(&dir), "有 .yushan/SYSTEM.md ⇒ true");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 自定义 prompt 短路**优先于** skill 注入：即便目录非空，也逐字返回该文件。
    ///
    /// 变异：把 skill 追加挪到 `find_custom_prompt` 短路**之前** ⇒ 本用例变红
    /// （用户全权接管的契约被破坏）。
    #[test]
    fn custom_prompt_short_circuit_ignores_non_empty_catalogue() {
        let _guard = env_lock();
        let _env = no_global_prompt();
        let dir = unique_dir("custom_skills");
        let system_dir = dir.join(".yushan");
        std::fs::create_dir_all(&system_dir).unwrap();
        std::fs::write(system_dir.join("SYSTEM.md"), "Custom system prompt").unwrap();

        let prompt = build_system_prompt(
            &dir,
            &[spec("read")],
            &one_skill("git-status", "查看仓库状态"),
        );
        assert_eq!(
            prompt, "Custom system prompt",
            "自定义 prompt 命中时不得追加 skill 段（既有契约逐字保留）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 能力边界声明**按工具表派生**：父表有派发工具 ⇒ 不说「no subagents」；
    /// 子表（无派发工具）⇒ 逐字保留。
    ///
    /// 变异：把 `skill_section` 的 `has_subagent` 参数写死 `false` ⇒ 第一条断言变红。
    #[test]
    fn capability_line_follows_the_tool_table() {
        let dir = unique_dir("cap_line");
        let cat = one_skill("arch-design", "多方案架构设计");

        let parent = build_system_prompt(&dir, &[spec("read"), spec("subagent")], &cat);
        assert!(
            parent.contains("CAN delegate tasks to subagents"),
            "父提示应声明可派发；prompt = {parent}"
        );
        assert!(
            !parent.contains("no subagents"),
            "父提示不得再说 no subagents；prompt = {parent}"
        );
        assert!(
            parent.contains(&format!(
                "At most {} subagents run in parallel",
                ys_component::DEFAULT_MAX_PARALLEL_TOOLS
            )),
            "并发槽位数须写进 guideline，且取自常量单一真值；prompt = {parent}"
        );
        // **异步派发**的指引只在 `subagent_async` 在场时出现（子表没有它）。
        //
        // 承重的三句：① 结果**自动送达、不要 poll**；② 模型先做完自己那部分时，
        //    回合**不会**结束 —— 它会继续跑到子 agent 回报（ADR-0055：父回合等得起）；
        //    ③ 不得谎报状态：既不必声称还在跑，也不要重派。
        //
        // 变异：删掉「do NOT poll for it」/「the turn keeps running until the sub-agent
        // reports back」/「say in one line what it found」⇒ 变红。
        let with_async = build_system_prompt(
            &dir,
            &[spec("read"), spec("subagent"), spec("subagent_async")],
            &cat,
        );
        assert!(
            with_async.contains("do NOT poll for it"),
            "必须说明「结果自动送达、不要 poll」；prompt = {with_async}"
        );
        assert!(
            with_async.contains("the turn keeps running until the sub-agent reports back"),
            "必须说明「父回合会等到子 agent 回报」—— 这是 ADR-0055 的行为契约；\
             prompt = {with_async}"
        );
        assert!(
            with_async.contains("never claim it is still running"),
            "必须禁止未核实就声称「还在跑」；prompt = {with_async}"
        );
        // 用户明确要求的「回复后立即汇报并继续」—— 汇报是义务，不是风格。
        assert!(
            with_async.contains("say in one line what it found and what you are doing next"),
            "必须要求结果到达后先报一句发现与下一步；prompt = {with_async}"
        );
        // **旧承诺必须消失**：旧文案说「结果在你下一个回合开头送达」，而那个
        // “next turn” 需要用户开口 —— 假契约已被 ADR-0055 取代。
        assert!(
            !with_async.contains("delivered at the start of your next turn"),
            "不得再承诺需要用户开口的「下一回合送达」；prompt = {with_async}"
        );
        let without = build_system_prompt(&dir, &[spec("read"), spec("subagent")], &cat);
        assert!(
            !without.contains("do NOT poll for it"),
            "`subagent_async` 不在场时不该出现它的指引；prompt = {without}"
        );

        // **成批派发**必须明写「放同一条消息里」——不是风格问题，是机制事实：
        // 同批进 `Segment::Parallel` 才真并发；一轮一个 = 串行 N 倍。
        //
        // 实测依据（turn 16）：3 个派发分 3 轮 ⇒ 1,831 s；同批只需 965 s。
        //
        // 变异：把「send ALL the `subagent` calls in ONE message」删掉，只留「scale」
        // 语义 ⇒ 变红。
        assert!(
            parent.contains("in ONE message")
                && parent.contains("one call per round runs them one after another"),
            "必须说明「同批并发、一轮一个串行」这个机制后果；prompt = {parent}"
        );

        let child = build_system_prompt(&dir, &[spec("read")], &cat);
        assert!(
            child.contains("no subagents"),
            "子提示（无 subagent 工具）应逐字保留那句；prompt = {child}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn subagent_guideline_block(prompt: &str) -> &str {
        let start = prompt
            .find("At most ")
            .expect("父提示应含 subagent 并发槽位 guideline");
        let rest = &prompt[start..];
        let end = rest
            .find("use it to save context, not to look busy")
            .expect("subagent guideline 段应以成本提醒结束")
            + "use it to save context, not to look busy".len();
        &rest[..end]
    }

    #[test]
    fn subagent_dispatch_guidelines_in_parent_prompt() {
        let _guard = env_lock();
        let _env = no_global_prompt();
        let dir = unique_dir("subagent_dispatch_parent");
        let cat = one_skill("arch-design", "多方案架构设计");
        let prompt = build_system_prompt(&dir, &[spec("read"), spec("subagent")], &cat);
        let block = subagent_guideline_block(&prompt);

        assert!(
            block.contains(
                "Before dispatching multiple subagents, check their dependencies and write scopes"
            ),
            "须要求派发前检查依赖与写入范围；block = {block}"
        );
        assert!(
            block.contains("wait for the prerequisite to finish"),
            "消费输出或验证修改的任务须等待前置完成；block = {block}"
        );
        assert!(
            block.contains("outputs are independent and their write scopes do not overlap"),
            "只有输出独立且写入范围不相交才可并行；block = {block}"
        );
        assert!(
            block.contains("serialize them when the scope is unclear"),
            "范围不明时须串行；block = {block}"
        );
        assert!(
            block.contains("read its diagnostic first")
                && block.contains("narrow the task and retry")
                && block.contains("report the failure to the user"),
            "失败或取消后须先读诊断，再缩窄重试或报告；block = {block}"
        );
        assert!(
            !block.contains(
                "parallel subagents share this workspace: never dispatch several in one turn if their write targets overlap"
            ),
            "旧窄写入规则必须删除；block = {block}"
        );
        assert!(
            block.contains(&format!(
                "At most {} subagents run in parallel in one turn",
                ys_component::DEFAULT_MAX_PARALLEL_TOOLS
            )),
            "并发槽位提醒必须保留；block = {block}"
        );
        assert!(
            block.contains("A subagent is not free")
                && block.contains("use it to save context, not to look busy"),
            "成本提醒必须保留；block = {block}"
        );
        assert!(
            !block.to_lowercase().contains("review") && !block.to_lowercase().contains("test"),
            "subagent guideline 不得硬编码 review/test 特例；block = {block}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 后台命令两条 guideline **只在 `bash_bg` 在场时**出现。
    ///
    /// 守卫形态与既有 `websearch` / `webfetch` / `subagent` 的「在场才出现」同形。
    /// **只看 `bash_bg`**：两条文案都只引用它（结果由 harness 送达，模型无需也不得轮询）；
    /// 缺了它，两条都是指向不存在工具的悬空指令。
    ///
    /// 反向断言（缺席时不得出现）是关键：少了它，一个「无条件把所有 guideline 都塞进去」
    /// 的实现也能过 —— 而那会让模型看到用不了的工具建议。
    ///
    /// 变异：① 把守卫改成无条件 push ⇒ 反向断言变红；
    /// ② 删掉「Do NOT poll for it」半句 ⇒ 关键片段断言变红；
    /// ③ 删掉「跨回合送达」那句 ⇒ 关键片段断言变红。
    #[test]
    fn background_command_guidelines_appear_only_when_bash_bg_present() {
        let _guard = env_lock();
        let _env = no_global_prompt();

        let parent = build_system_prompt(
            Path::new("/tmp"),
            &[spec("read"), spec("bash"), spec("bash_bg")],
            &no_skills(),
        );

        let first = "- Run a command that takes more than a few seconds with bash_bg";
        let second = "- Never use sleep to wait for something";
        assert!(
            parent.contains(first),
            "父表（有 bash_bg）应给「长命令后台化 + 结果自动送达」的指引；prompt = {parent}"
        );
        assert!(
            parent.contains(second),
            "父表（有 bash_bg）应给「不用 sleep 假装在等」的指引；prompt = {parent}"
        );
        // 两条**按序**：后台化在前，禁 sleep 在后。
        let first_at = parent.find(first).expect("第一条应出现");
        let second_at = parent.find(second).expect("第二条应出现");
        assert!(
            first_at < second_at,
            "两条 guideline 应按序出现（后台化在前，禁 sleep 在后）；prompt = {parent}"
        );

        // 既有 guideline 全是**纯文本**：无 markdown 强调标记、无句末句号 ——
        // 提示词里出现 `**` 是噪声（模型读到的是字面字符，不是强调）。
        for line in parent
            .lines()
            .filter(|line| line.starts_with(first) || line.starts_with(second))
        {
            assert!(
                !line.contains("**"),
                "guideline 不得带 markdown 强调标记；line = {line}"
            );
            assert!(
                !line.ends_with('.'),
                "guideline 不带句末句号（与既有条目同风格）；line = {line}"
            );
        }

        // 关键片段：完成即送达 / **显式禁轮询** / 跨回合补投 / 只提示通知不提示取。
        for fragment in [
            "its result is delivered to you automatically when it finishes",
            "Do NOT poll for it",
            "the result arrives at the start of your next turn",
            "you will be notified when it finishes",
        ] {
            assert!(
                parent.contains(fragment),
                "父表指引须含关键片段 `{fragment}`；prompt = {parent}"
            );
        }

        // 清单里必须有 `bash_bg` 的**专属** snippet：否则会退化成 `Custom tool`，
        // 模型在清单那行看不出它与 `bash` 的差别（漂移守卫的同形要求）。
        let lines = listed_tool_lines(&parent);
        let line = lines
            .iter()
            .find(|line| line.starts_with("- bash_bg: "))
            .unwrap_or_else(|| panic!("清单里应有 `bash_bg`；lines = {lines:?}"));
        assert!(
            !line.ends_with("Custom tool"),
            "`bash_bg` 不得退化成兜底 snippet；line = {line}"
        );

        // 完全没有 bg 工具 ⇒ 两条都不出现。
        let plain = build_system_prompt(
            Path::new("/tmp"),
            &[spec("read"), spec("bash")],
            &no_skills(),
        );
        assert!(
            !plain.contains(first) && !plain.contains(second),
            "无 bash_bg 时两条 guideline 都不得出现；prompt = {plain}"
        );
    }

    /// 子 agent 的工具表（基础 9 元）**不含 bg 工具** ⇒ 天然看不到这两条 guideline。
    ///
    /// 这与 `subagent` 那组的「在场才出现」同形：子表少一项，父表多一项，
    /// 提示词各说各话，互不越界。
    #[test]
    fn background_command_guidelines_absent_from_child_prompt() {
        let _guard = env_lock();
        let _env = no_global_prompt();

        let child = build_system_prompt(
            Path::new("/tmp"),
            &[
                spec("read"),
                spec("write"),
                spec("edit"),
                spec("bash"),
                spec("grep"),
                spec("find"),
                spec("ls"),
                spec("websearch"),
                spec("webfetch"),
            ],
            &no_skills(),
        );

        assert!(
            !child.contains("with bash_bg"),
            "子提示不得出现后台命令指引；prompt = {child}"
        );
        // 提示词只引用运行时真有的工具：子表没有 `bash_bg`，也就不得出现 `bash_poll` 这类
        // 悬空名字（模型会照着它去调一个不存在的工具）。
        assert!(
            !child.contains("bash_poll"),
            "子提示不得出现 bash_poll；prompt = {child}"
        );
        assert!(
            !child.contains("Never use sleep to wait for something"),
            "子提示不得出现「不用 sleep」指引；prompt = {child}"
        );
    }

    #[test]
    fn subagent_dispatch_guidelines_absent_from_child_prompt() {
        let _guard = env_lock();
        let _env = no_global_prompt();
        let dir = unique_dir("subagent_dispatch_child");
        let cat = one_skill("arch-design", "多方案架构设计");
        let prompt = build_system_prompt(&dir, &[spec("read")], &cat);

        for phrase in [
            "Before dispatching multiple subagents",
            "wait for the prerequisite to finish",
            "write scopes do not overlap",
            "serialize them when the scope is unclear",
            "read its diagnostic first",
        ] {
            assert!(
                !prompt.contains(phrase),
                "子提示不得含派发 guideline `{phrase}`；prompt = {prompt}"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 派发探索的**触发条件**只在父表有 `subagent` 时渲染，且必须写明
    /// 「多个独立问题 + 一条消息成批 + 小查找自己搜」。
    ///
    /// 变异：① 删掉 `mode=\"explore\"` 或 `in ONE message` ⇒ 正向断言变红；
    /// ② 去掉 `tools.contains(&"subagent")` 门控 ⇒ 反向断言变红。
    #[test]
    fn delegate_exploration_guideline_gated_on_subagent() {
        let _guard = env_lock();
        let _env = no_global_prompt();

        let parent = build_system_prompt(
            Path::new("/tmp"),
            &[spec("read"), spec("grep"), spec("subagent")],
            &no_skills(),
        );
        assert!(
            parent
                .contains("Delegate exploration only when you have several independent questions"),
            "父表有 subagent 时应渲染派发探索指引；prompt = {parent}"
        );
        assert!(
            parent.contains("mode=\"explore\""),
            "指引须点明 `mode=\"explore\"`；prompt = {parent}"
        );
        assert!(
            parent.contains("in ONE message"),
            "指引须要求成批放进一条消息；prompt = {parent}"
        );
        assert!(
            parent.contains("For one or two small lookups, search directly"),
            "指引须说明小查找自己搜更快；prompt = {parent}"
        );

        let child = build_system_prompt(
            Path::new("/tmp"),
            &[spec("read"), spec("grep")],
            &no_skills(),
        );
        assert!(
            !child.contains("Delegate exploration only when"),
            "无 subagent 时不得渲染派发探索指引；prompt = {child}"
        );
    }

    // ─────────────────────────── 探索子 agent（mode=explore） ───────────────────────────

    /// 探索提示的差异：探索角色行、`path:line` 输出契约、**不含**叙述条目。
    ///
    /// 反向断言是关键：`narrate = false` 的实现若仍渲染「过程叙述」，子 agent 会
    /// 被要求向用户汇报进展 —— 而它的读者是父模型，那条指令只增成本。
    ///
    /// 变异 1：把角色行改回 `You are a coding agent` ⇒ 角色断言变红。
    /// 变异 2：删掉 `EXPLORE_REPORT_FORMAT` ⇒ `path:line — fact` / `at most 5 findings` 变红。
    /// 变异 3：`render_guidelines` 忽略 `narrate` 参数 ⇒ 叙述断言变红。
    #[test]
    fn explore_prompt_has_role_and_report_contract_without_narration() {
        let _guard = env_lock();
        let _env = no_global_prompt();
        let dir = unique_dir("explore_prompt");

        let specs: Vec<ToolSpec> = [
            "read",
            "bash",
            "grep",
            "find",
            "ls",
            "websearch",
            "webfetch",
        ]
        .iter()
        .map(|name| spec(name))
        .collect();
        let prompt = build_explore_prompt(&dir, &specs, &no_skills());

        assert!(
            prompt.contains(
                "You are an exploration agent. You investigate a codebase or its documents \
                 and report findings with exact citations. You do not modify anything."
            ),
            "探索角色行必须逐字在场；prompt = {prompt}"
        );
        assert!(
            prompt.contains("path:line — fact"),
            "输出契约必须给出 `path:line — fact` 形状；prompt = {prompt}"
        );
        assert!(
            prompt.contains("at most 5 findings"),
            "输出契约必须有界（至多 5 条发现）；prompt = {prompt}"
        );
        assert!(
            !prompt.contains("listing what you searched for"),
            "输出契约不得再要求穷举「搜过哪里」；prompt = {prompt}"
        );
        assert!(
            !prompt.contains("Before the first tool call of a new request"),
            "探索子 agent 不得出现「过程叙述」guideline（其读者不是用户）；prompt = {prompt}"
        );
        // 其余 guideline 仍在：只去掉叙述那一条，不是整份列表被跳过。
        assert!(
            prompt.contains("Explore with one command"),
            "只读工具在场时聚合探索指引仍须渲染；prompt = {prompt}"
        );
        // 工具清单沿用同一格式（`- name: snippet`）。
        assert!(
            prompt.contains("- read: Read file contents"),
            "工具清单须沿用既有格式；prompt = {prompt}"
        );
        // cwd 段仍在最前。
        assert!(
            prompt.starts_with(&format!("Current working directory: {}", dir.display())),
            "cwd 段须保留且在最前；prompt = {prompt}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 探索提示**不加载**项目上下文：AGENTS.md 段是每轮前缀成本（约 6.8k token），
    /// 且会诱导探索 agent「按规范做事」而不是「尽快定位」。
    ///
    /// 正向对照（主 agent 仍带该段）是必要的：否则磁盘上没有 AGENTS.md 时，
    /// 「不含 `<project_context>`」会因为没东西可加而假绿。
    ///
    /// 变异：把 `build_explore_prompt` 里删掉的 `load_project_context` 段加回去 ⇒
    /// 第一条断言变红。
    #[test]
    fn explore_prompt_skips_project_context_but_keeps_cwd() {
        let _guard = env_lock();
        let _env = no_global_prompt();
        let dir = unique_dir("explore_no_context");
        std::fs::write(dir.join("AGENTS.md"), "PROJECT-CONVENTION-MARKER").unwrap();

        let specs = [spec("read"), spec("bash")];
        let explore = build_explore_prompt(&dir, &specs, &no_skills());
        let main = build_system_prompt(&dir, &specs, &no_skills());

        assert!(
            main.contains("<project_context>") && main.contains("PROJECT-CONVENTION-MARKER"),
            "对照：主 agent 必须仍加载项目上下文；prompt = {main}"
        );
        assert!(
            !explore.contains("<project_context>")
                && !explore.contains("PROJECT-CONVENTION-MARKER"),
            "探索提示不得加载项目上下文；prompt = {explore}"
        );
        assert!(
            explore.starts_with(&format!("Current working directory: {}", dir.display())),
            "cwd 段须保留且在最前；prompt = {explore}"
        );
        assert!(
            explore.contains("You are an exploration agent."),
            "角色行须仍在；prompt = {explore}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 探索提示**不走**自定义 prompt 短路：用户给主 agent 的 `SYSTEM.md` 不得接管
    /// 探索子 agent 的输出契约。
    ///
    /// 变异：在 `build_explore_prompt` 开头加 `if let Some(custom) = find_custom_prompt(cwd)
    /// { return custom; }` ⇒ 本用例变红（探索角色行消失）。
    #[test]
    fn explore_prompt_ignores_custom_system_prompt() {
        let _guard = env_lock();
        let _env = no_global_prompt();
        let dir = unique_dir("explore_custom");
        let system_dir = dir.join(".yushan");
        std::fs::create_dir_all(&system_dir).unwrap();
        std::fs::write(system_dir.join("SYSTEM.md"), "Custom system prompt").unwrap();

        let prompt = build_explore_prompt(&dir, &[spec("read")], &no_skills());
        assert!(
            prompt.contains("You are an exploration agent."),
            "探索提示不得被 SYSTEM.md 短路；prompt = {prompt}"
        );
        assert_ne!(prompt, "Custom system prompt");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 探索提示带**工具形状的检索方法**：一条聚合命令、16 KiB 上限、`rg -l`、不越界。
    ///
    /// 16 KiB 是模型无法从工具描述推断的事实（`grep` 有硬上限，加大 `limit` 无用）。
    ///
    /// 变异：删掉 `EXPLORE_SEARCH_METHOD` 或其中任一条 ⇒ 对应断言变红。
    #[test]
    fn explore_prompt_has_tool_shaped_search_method() {
        let _guard = env_lock();
        let _env = no_global_prompt();
        let dir = unique_dir("explore_search_method");

        let specs = [spec("read"), spec("grep"), spec("bash")];
        let prompt = build_explore_prompt(&dir, &specs, &no_skills());
        for fragment in [
            "How to search:",
            "16 KiB",
            "rg -l",
            "do not expand the audit beyond it",
        ] {
            assert!(
                prompt.contains(fragment),
                "探索提示须含检索方法片段 `{fragment}`；prompt = {prompt}"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 主 agent 的提示主体**逐字节不变**（`mode` 只影响子 agent）。
    ///
    /// 观测手段：把临时 cwd 归一成 `<cwd>` 后取 `build_system_prompt` 的指纹。
    /// 只哈希 `<project_context>` 之前的段 —— 那一段由本函数生成，不含运行环境内容
    /// （项目上下文来自磁盘上的 AGENTS.md，随机器而变，不属本函数的字节契约）。
    ///
    /// 变异：改角色行、工具清单形状、任一条 guideline 的文案或顺序 ⇒ 指纹变化。
    #[test]
    fn main_system_prompt_body_is_byte_stable() {
        let _guard = env_lock();
        let _env = no_global_prompt();
        let dir = unique_dir("main_prompt_bytes");

        let specs: Vec<ToolSpec> = EXPECTED_ORDER.iter().map(|name| spec(name)).collect();
        let prompt = build_system_prompt(&dir, &specs, &no_skills());
        let body = prompt
            .split("\n\n<project_context>")
            .next()
            .expect("split 至少返回一段");
        let normalized = body.replace(&dir.display().to_string(), "<cwd>");
        assert_eq!(
            fnv1a64(normalized.as_bytes()),
            0xb238d36795e45d24u64,
            "主 agent 提示主体漂移：确认是有意改动，再更新此锚点"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FNV-1a 64 —— 指纹只用于发现字节漂移，不要求抗碰撞。
    fn fnv1a64(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in bytes {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }
}
