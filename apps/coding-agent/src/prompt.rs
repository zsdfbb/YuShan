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
/// # skill 目录段（纯追加，设计 §选择理由 F）
///
/// 段落在**整条提示的最末尾**（`Current working directory: …` 之后），故
/// **空 `catalogue` 的输出是任何非空 `catalogue` 输出的逐字节前缀** ——
/// 这是「同一台机器上先后两次启动的提示前缀稳定」的可测形式（严格前缀不变量）。
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

    let mut prompt = format!(
        r#"You are a coding agent. You help users with software engineering tasks.

Available tools:
{tool_list}

Guidelines:
{guidelines}"#
    );

    // 3. 追加项目上下文（AGENTS.md / YUSHAN.md）
    let context_files = load_project_context(cwd);
    if !context_files.is_empty() {
        prompt.push_str("\n\n<project_context>\n\n");
        prompt.push_str("Project-specific instructions and guidelines:\n\n");
        for ctx in &context_files {
            prompt.push_str(&format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
                ctx.path.display(),
                ctx.content
            ));
        }
        prompt.push_str("</project_context>");
    }

    prompt.push_str(&format!("\n\nCurrent working directory: {}", cwd.display()));

    // 4. 追加 skill 目录段 —— **必须留在最末尾**（见函数文档的严格前缀不变量）。
    //    门控：有 `read`（skill 正文靠它加载）且目录非空；否则**一字不加**。
    if tools.iter().any(|t| t.name == "read") && !catalogue.lines.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&skill_section(&catalogue.lines));
    }

    prompt
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

/// 工具的一句话描述，按 **name** 查表。
///
/// 名字不在表里时回退到 `"Custom tool"`（外部工具 / 未来新增工具都能列出而不报错）。
fn tool_snippet(name: &str) -> &'static str {
    match name {
        "read" => "Read file contents",
        "write" => "Create or overwrite files",
        "edit" => "Make precise file edits with exact text replacement",
        "bash" => "Execute bash commands",
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
        _ => "Custom tool",
    }
}

fn tool_guidelines(tools: &[&str]) -> String {
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
    if tools.contains(&"read") {
        guidelines.push("Use read to examine files instead of cat or sed".to_string());
    }
    if tools.contains(&"grep") {
        guidelines.push(
            "Use grep to search file contents instead of running grep through bash".to_string(),
        );
    }
    if tools.contains(&"find") {
        guidelines
            .push("Use find to locate files instead of running find through bash".to_string());
    }
    if tools.contains(&"ls") {
        guidelines
            .push("Use ls to list directories instead of running ls through bash".to_string());
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
    if path.exists() && !seen.contains(path) {
        if let Ok(content) = std::fs::read_to_string(path) {
            files.push(ContextFile {
                path: path.to_path_buf(),
                content,
            });
            seen.insert(path.to_path_buf());
        }
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
        let with_skills = build_system_prompt(&dir, &specs, &one_skill("git-status", "查看仓库状态"));

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
            prompt.contains("- arch-design: 多方案架构设计 (location: /skills/arch-design/SKILL.md)"),
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
            empty.ends_with(&format!("Current working directory: {}", dir.display())),
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
}
