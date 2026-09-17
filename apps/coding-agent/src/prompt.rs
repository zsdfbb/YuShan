use std::path::Path;

use ys_tool::ToolSpec;

/// 用工具片段和动态 guideline 构建 system prompt。
///
/// # 工具清单从 `tools` 派生
///
/// 清单**逐字按 `tools` 的顺序**渲染 `- {name}: {snippet}` —— 保序由工具表装配端
/// （`toolset::build_tools` → `ToolRegistry`，见
/// `docs/design-plans/2026-09-16-search-tools.md` B1）保证，本函数不排序、
/// 不查自己的名录：`tools` 里有什么就列什么。这样提示词与注册表**不可能漂移**。
pub fn build_system_prompt(cwd: &Path, tools: &[ToolSpec]) -> String {
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
    prompt
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

    // 从 cwd 逐级向父目录遍历
    let mut current = cwd.to_path_buf();
    loop {
        for name in &["AGENTS.override.md", "AGENTS.md", "YUSHAN.md", "CLAUDE.md"] {
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
        let prompt = build_system_prompt(Path::new("/tmp"), &specs);

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
            let prompt = build_system_prompt(Path::new("/tmp"), &specs);

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

        let (tools, _hint) = crate::toolset::build_tools(&dir);
        let names: Vec<String> = tools.iter().map(|t| t.spec().name).collect();

        // 先确认取到的是真实工具表（空表会让下面的循环平凡通过）。
        let actual: Vec<&str> = names.iter().map(String::as_str).collect();
        assert_eq!(actual, EXPECTED_ORDER, "应取到装配端的真实 7 元工具表");

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

    /// `grep` / `find` / `ls` 的 snippet **逐字**钉死（T11 review 点名的无守卫项）。
    ///
    /// 非兜底断言（上一用例）只能证明「有专属文案」，证不了「文案说对了」；
    /// 这三条是模型真正读到的检索/列目录指引，故用字面量锁文本。
    ///
    /// 变异：把 `"grep"` 分支的文案改成别的（哪怕仍非兜底）→ 本用例变红。
    #[test]
    fn search_and_list_snippets_are_verbatim() {
        assert_eq!(
            tool_snippet("grep"),
            "Search file contents for a pattern (respects .gitignore when ripgrep is available)"
        );
        assert_eq!(tool_snippet("find"), "Search for files by glob pattern");
        assert_eq!(tool_snippet("ls"), "List directory contents");
        // 兜底分支本身也要在（外部工具靠它列出而不报错）。
        assert_eq!(tool_snippet("__no_such_tool__"), "Custom tool");
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
        let prompt = build_system_prompt(&tmp, &specs);
        assert_eq!(prompt, "Custom system prompt");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
