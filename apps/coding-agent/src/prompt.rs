use std::path::{Path, PathBuf};

/// Build system prompt with tool snippets and dynamic guidelines.
pub fn build_system_prompt(cwd: &Path) -> String {
    // 1. Check for custom prompt files
    if let Some(custom) = find_custom_prompt(cwd) {
        return custom;
    }

    // 2. Build default prompt
    let tools = ["read", "write", "edit", "bash"];
    let tool_list: Vec<String> = tools
        .iter()
        .map(|name| format!("- {name}: {}", tool_snippet(name)))
        .collect();

    let tool_list = tool_list.join("\n");
    let guidelines = tool_guidelines(&tools);

    let mut prompt = format!(
        r#"You are a coding agent. You help users with software engineering tasks.

Available tools:
{tool_list}

Guidelines:
{guidelines}"#
    );

    // 3. Append project context (AGENTS.md / YUSHAN.md)
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

fn tool_snippet(name: &str) -> &'static str {
    match name {
        "read" => "Read file contents",
        "write" => "Create or overwrite files",
        "edit" => "Make precise file edits with exact text replacement",
        "bash" => "Execute bash commands",
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

    // Global context
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        try_load(
            &std::path::PathBuf::from(home).join(".yushan/AGENTS.md"),
            &mut files,
            &mut seen,
        );
    }

    // Walk up from cwd
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

    files.reverse(); // Ancestors from far to near
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
    // Check .yushan/SYSTEM.md
    let project_path = cwd.join(".yushan/SYSTEM.md");
    if project_path.exists() {
        return std::fs::read_to_string(project_path).ok();
    }
    // Check ~/.yushan/SYSTEM.md
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let global_path = std::path::PathBuf::from(home).join(".yushan/SYSTEM.md");
        if global_path.exists() {
            return std::fs::read_to_string(global_path).ok();
        }
    }
    None
}

/// Render cwd with $HOME prefix replaced by `~/`. Falls back to absolute path if cwd is outside $HOME.
pub fn format_cwd_tilde(cwd: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let home_path = PathBuf::from(home);
        if let Ok(rel) = cwd.strip_prefix(&home_path) {
            return format!("~/{}", rel.display());
        }
    }
    cwd.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_prompt_contains_tools() {
        let prompt = build_system_prompt(Path::new("/tmp"));
        assert!(prompt.contains("read"));
        assert!(prompt.contains("write"));
        assert!(prompt.contains("edit"));
        assert!(prompt.contains("bash"));
        assert!(prompt.contains("coding agent"));
    }

    #[test]
    fn test_custom_prompt_takes_precedence() {
        let tmp = std::env::temp_dir().join("yushan_prompt_test");
        let system_dir = tmp.join(".yushan");
        std::fs::create_dir_all(&system_dir).unwrap();
        std::fs::write(system_dir.join("SYSTEM.md"), "Custom system prompt").unwrap();

        let prompt = build_system_prompt(&tmp);
        assert_eq!(prompt, "Custom system prompt");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_format_cwd_tilde_inside_home() {
        unsafe {
            std::env::set_var("HOME", "/test/home");
            std::env::remove_var("USERPROFILE");
        }
        let cwd = PathBuf::from("/test/home/foo");
        assert_eq!(format_cwd_tilde(&cwd), "~/foo");
    }

    #[test]
    fn test_format_cwd_tilde_at_home() {
        unsafe {
            std::env::set_var("HOME", "/test/home");
            std::env::remove_var("USERPROFILE");
        }
        let cwd = PathBuf::from("/test/home");
        assert_eq!(format_cwd_tilde(&cwd), "~/");
    }

    #[test]
    fn test_format_cwd_tilde_outside_home() {
        unsafe {
            std::env::set_var("HOME", "/test/home");
            std::env::remove_var("USERPROFILE");
        }
        let cwd = PathBuf::from("/tmp");
        assert_eq!(format_cwd_tilde(&cwd), "/tmp");
    }

    #[test]
    fn test_format_cwd_tilde_no_home() {
        unsafe {
            std::env::remove_var("HOME");
            std::env::remove_var("USERPROFILE");
        }
        let cwd = PathBuf::from("/tmp");
        assert_eq!(format_cwd_tilde(&cwd), "/tmp");
    }
}
