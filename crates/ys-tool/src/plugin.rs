//! 工具装配缝：`ToolPlugin` / `ToolPrompt` / `GuidelineBlock` —— 纯数据 + 纯函数。
//!
//! 装配缝把「加一个工具」的连锁点（工具表位次、system prompt 的 snippet 片段、
//! guideline 块）收敛成**一处声明**：第三方 crate 导出 `pub fn plugin() -> ToolPlugin`，
//! 装配方加一行登记即可。
//!
//! **本模块只有数据与排序**：没有任何运行时回调语义（不建 hook 系统，ADR-0014），
//! 不做 IO，也不依赖 tokio —— 插件的顺序与内容在编译期固定。
//!
//! # 两条彼此独立的序（缓存前缀稳定性的全部要点）
//!
//! - **工具序**：`ToolPlugin::order` 升序 ⇒ `tools()` 与 tools JSON 的顺序。
//! - **guideline 序**：每块自带全局 [`GuidelineBlock::slot`]，与工具序、插件序**都无关**。
//!
//! 两者不能合并成一条序：guideline 从 `edit` 开始而工具表从 `read` 开始；且 `read` 的三块
//! 被 `grep` 的块插在中间（`grep` 的门控嵌在 `read` 组的门控体内）。按工具序或插件序展开
//! 都会重排字节 ⇒ 每轮固定前缀变化 ⇒ 所有用户的 prompt cache 失效。

use std::collections::HashMap;

use crate::{Tool, ToolError};

/// 一个工具连同它对**每轮固定前缀**的全部贡献。
///
/// `order` / `slot` 都必须来自**编译期常量**：它们一变，prompt 前缀字节就变。
/// 不得来自 env / 登录态 / 探测结果（否则前缀逐进程抖动）。
pub struct ToolPlugin {
    /// **工具位次**（升序折表），决定 `tools()` 与 tools JSON 的顺序。
    ///
    /// 同一批插件内必须互不相同 —— 相同则顺序取决于声明先后（隐式规则），
    /// 由 [`sort_plugins`] 校验。
    pub order: u16,
    /// 本插件提供的工具；名字不得与其他插件重复（[`sort_plugins`] 校验）。
    pub tools: Vec<Box<dyn Tool>>,
    /// 工具在 system prompt 里的文本贡献（snippet + guideline 块）。
    pub prompts: Vec<ToolPrompt>,
}

impl ToolPlugin {
    /// 构造一个不含 prompt 贡献的插件。
    pub fn new(order: u16, tools: Vec<Box<dyn Tool>>) -> Self {
        Self {
            order,
            tools,
            prompts: Vec::new(),
        }
    }

    /// 附上 prompt 贡献（snippet 与 guideline 块）。
    pub fn with_prompts(mut self, prompts: Vec<ToolPrompt>) -> Self {
        self.prompts = prompts;
        self
    }

    /// 本插件声明的全部 guideline 块，逐项带上**归属工具名** —— 供 [`expand_guidelines`]。
    ///
    /// 归属名是 `Gate::ThisTool` 的判定依据（块自己不知道属于哪个工具）。
    pub fn guideline_entries(&self) -> impl Iterator<Item = (&str, &GuidelineBlock)> + '_ {
        self.prompts.iter().flat_map(|p| p.guideline_entries())
    }
}

impl std::fmt::Debug for ToolPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolPlugin")
            .field("order", &self.order)
            .field(
                "tools",
                &self.tools.iter().map(|t| t.spec().name).collect::<Vec<_>>(),
            )
            .field("prompts", &self.prompts)
            .finish()
    }
}

/// 工具对 system prompt 的文本贡献。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPrompt {
    /// 归属工具名：`snippet` 按它查表，`Gate::ThisTool` 也按它判定。
    pub tool: String,
    /// `- {tool}: {snippet}` 的 snippet 部分。
    ///
    /// **必须独立于 [`crate::ToolSpec::description`]**：两者文本不同，复用会改前缀字节、
    /// 废掉既有的缓存基线。
    pub snippet: String,
    /// 本工具贡献的 guideline 块。空 = 该工具无 guideline。
    ///
    /// 块在列表里的位置**不决定**渲染位置 —— 渲染序由每块的 [`GuidelineBlock::slot`] 给出。
    pub guidelines: Vec<GuidelineBlock>,
}

impl ToolPrompt {
    /// 构造一个不带 guideline 的 prompt 片段。
    pub fn new(tool: impl Into<String>, snippet: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            snippet: snippet.into(),
            guidelines: Vec::new(),
        }
    }

    /// 附上 guideline 块。
    pub fn with_guidelines(mut self, guidelines: Vec<GuidelineBlock>) -> Self {
        self.guidelines = guidelines;
        self
    }

    /// 本工具声明的全部 guideline 块，逐项带上归属工具名 —— 供 [`expand_guidelines`]。
    pub fn guideline_entries(&self) -> impl Iterator<Item = (&str, &GuidelineBlock)> + '_ {
        let tool = self.tool.as_str();
        self.guidelines.iter().map(move |b| (tool, b))
    }
}

/// 一块 guideline（渲染时每行加 `- ` 前缀）。
///
/// **不建模成「每工具一个 `Vec<String>`」**：现有 13 个门控块里，`narrate` 块夹在
/// `read` 组与 `websearch` 之间，`grep` 块嵌在 `read` 组的门控体内，且有 3 块不是单工具门控。
/// 简化成「每工具一串」会静默改变字节顺序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuidelineBlock {
    /// **全局渲染位次**（升序）。与插件 `order`、工具序**无关** —— 见模块文档。
    ///
    /// 同一批块内必须唯一（[`sort_plugins`] / [`expand_guidelines`] 校验）：
    /// 重复会让渲染序取决于传入顺序。
    pub slot: u16,
    /// 门控：哪些工具在场时才渲染本块。
    pub gate: Gate,
    /// 生效范围：主 agent / 子 agent。
    pub scope: Scope,
    /// 正文行（不含 `- ` 前缀）。
    pub lines: Vec<String>,
}

impl GuidelineBlock {
    /// 完整构造。
    pub fn new(slot: u16, gate: Gate, scope: Scope, lines: Vec<String>) -> Self {
        Self {
            slot,
            gate,
            scope,
            lines,
        }
    }

    /// 最常见的形态：本工具在场即渲染、主/子 agent 都出。
    pub fn for_tool(slot: u16, lines: Vec<String>) -> Self {
        Self::new(slot, Gate::ThisTool, Scope::Always, lines)
    }

    /// 本块在当前装配下是否渲染。
    ///
    /// `owner` 是声明本块的工具名（`Gate::ThisTool` 的判定依据）；`tools` 是**在场**的工具名；
    /// `narrate = false` 表示子 agent。
    pub fn is_active(&self, owner: &str, tools: &[&str], narrate: bool) -> bool {
        self.scope.allows(narrate) && self.gate.matches(owner, tools)
    }
}

/// guideline 块的门控条件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// 本工具在场（大多数块）。
    ThisTool,
    /// 列出的工具**任一**在场。例：`edit || write`、`read` 组 4 工具、
    /// narrate 块的 7 个工作型工具。
    AnyOf(&'static [&'static str]),
}

impl Gate {
    /// 门控是否满足。`owner` 是本块的归属工具名。
    pub fn matches(&self, owner: &str, tools: &[&str]) -> bool {
        match self {
            Gate::ThisTool => tools.contains(&owner),
            Gate::AnyOf(names) => names.iter().any(|n| tools.contains(n)),
        }
    }
}

/// guideline 块的生效范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// 主 agent 与子 agent 都渲染。
    Always,
    /// 仅主 agent 渲染（`narrate = true`）。
    ///
    /// 今天只有「过程叙述」那一块：它服务于面向用户的进展可见性，而子 agent 的输出
    /// 只回给父模型。
    MainOnly,
}

impl Scope {
    /// 该范围在当前调用点是否生效。`narrate = false` 表示子 agent。
    pub fn allows(self, narrate: bool) -> bool {
        match self {
            Scope::Always => true,
            Scope::MainOnly => narrate,
        }
    }
}

/// 按 `order` 升序折表，并校验三条**声明期不变量**（任一条被违反都是装配错误）。
///
/// 校验：工具名跨插件唯一（错误信息点名两个插件位次）、插件 `order` 唯一、
/// guideline `slot` 全局唯一。排序用稳定排序 —— 校验通过后同 `order` 已不可能出现。
pub fn sort_plugins(plugins: Vec<ToolPlugin>) -> Result<Vec<ToolPlugin>, ToolError> {
    let mut owner_of: HashMap<String, usize> = HashMap::new();
    for (idx, plugin) in plugins.iter().enumerate() {
        for tool in &plugin.tools {
            let name = tool.spec().name;
            if let Some(&first) = owner_of.get(&name) {
                return Err(ToolError::InvalidInput(format!(
                    "duplicate tool name: {name} (declared by plugin #{} and plugin #{})",
                    first + 1,
                    idx + 1
                )));
            }
            owner_of.insert(name, idx);
        }
    }

    let mut order_of: HashMap<u16, usize> = HashMap::new();
    for (idx, plugin) in plugins.iter().enumerate() {
        if let Some(&first) = order_of.get(&plugin.order) {
            return Err(ToolError::InvalidInput(format!(
                "duplicate plugin order: {} (declared by plugin #{} and plugin #{})",
                plugin.order,
                first + 1,
                idx + 1
            )));
        }
        order_of.insert(plugin.order, idx);
    }

    let entries: Vec<(&str, &GuidelineBlock)> =
        plugins.iter().flat_map(|p| p.guideline_entries()).collect();
    validate_slots(&entries)?;

    let mut sorted = plugins;
    sorted.sort_by_key(|p| p.order);
    Ok(sorted)
}

/// 把一组 guideline 块按 `slot` **全局升序**展开成 `- {line}` 行，并按 `gate` / `scope` 过滤。
///
/// 纯函数：入参是「归属工具名 + 块」，`tools` 为**在场**的工具名（顺序无关，只用于门控），
/// `narrate = false` 表示子 agent（过滤 [`Scope::MainOnly`]）。
///
/// **`slot` 重复 ⇒ `Err`**：重复会让渲染序取决于传入顺序，前缀字节随之抖动。
/// 校验在过滤**之前**做 —— 被门控掉的块今天不渲染，但门控条件一变就会暴露，
/// 属声明期错误。
pub fn expand_guidelines<'a>(
    blocks: impl IntoIterator<Item = (&'a str, &'a GuidelineBlock)>,
    tools: &[&str],
    narrate: bool,
) -> Result<Vec<String>, ToolError> {
    let all: Vec<(&str, &GuidelineBlock)> = blocks.into_iter().collect();
    validate_slots(&all)?;

    let mut active: Vec<&GuidelineBlock> = all
        .iter()
        .filter(|(owner, block)| block.is_active(owner, tools, narrate))
        .map(|(_, block)| *block)
        .collect();
    active.sort_by_key(|block| block.slot);

    Ok(active
        .iter()
        .flat_map(|block| block.lines.iter().map(|line| format!("- {line}")))
        .collect())
}

/// `slot` 全局唯一校验；错误信息点名两个归属工具。
fn validate_slots(blocks: &[(&str, &GuidelineBlock)]) -> Result<(), ToolError> {
    let mut owner_of: HashMap<u16, &str> = HashMap::new();
    for (owner, block) in blocks {
        if let Some(first) = owner_of.get(&block.slot) {
            return Err(ToolError::InvalidInput(format!(
                "duplicate guideline slot: {} (declared by tool \"{first}\" and tool \"{owner}\")",
                block.slot
            )));
        }
        owner_of.insert(block.slot, owner);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolContext, ToolResult, ToolSpec};

    struct StubTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for StubTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(self.0, "stub", serde_json::json!({}))
        }

        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            unimplemented!()
        }
    }

    fn tool(name: &'static str) -> Box<dyn Tool> {
        Box::new(StubTool(name))
    }

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn test_expand_guidelines_sorts_by_slot() {
        // 乱序传入 ⇒ 输出按 slot 升序。
        // 变异：去掉 `sort_by_key` ⇒ 顺序变成 30,10,20 ⇒ 红。
        let a = GuidelineBlock::for_tool(30, lines(&["third"]));
        let b = GuidelineBlock::for_tool(10, lines(&["first"]));
        let c = GuidelineBlock::for_tool(20, lines(&["second"]));

        let out =
            expand_guidelines([("read", &a), ("read", &b), ("read", &c)], &["read"], true).unwrap();
        assert_eq!(out, vec!["- first", "- second", "- third"]);
    }

    #[test]
    fn test_expand_guidelines_keeps_global_slot_order_across_plugins() {
        // 既有事实：`read` 的三块被 `grep` 的块插在中间 ⇒ 渲染序由 slot 给出，
        // 不是「按插件逐个吐出」。变异：按插件/按工具分组展开 ⇒ 4,6,7,5 ⇒ 红。
        let read_group = Gate::AnyOf(&["read", "grep", "find", "ls"]);
        let read4 = GuidelineBlock::new(4, read_group, Scope::Always, lines(&["read group head"]));
        let read6 = GuidelineBlock::new(6, read_group, Scope::Always, lines(&["read group tail"]));
        let read7 = GuidelineBlock::new(
            7,
            Gate::AnyOf(&["read", "grep"]),
            Scope::Always,
            lines(&["start wide"]),
        );
        let grep5 = GuidelineBlock::for_tool(5, lines(&["grep caps at 16 KiB"]));

        let read = ToolPlugin::new(10, vec![]).with_prompts(vec![
            ToolPrompt::new("read", "r").with_guidelines(vec![read4, read6, read7]),
        ]);
        let grep = ToolPlugin::new(11, vec![]).with_prompts(vec![
            ToolPrompt::new("grep", "g").with_guidelines(vec![grep5]),
        ]);

        let entries: Vec<(&str, &GuidelineBlock)> = read
            .guideline_entries()
            .chain(grep.guideline_entries())
            .collect();
        let out = expand_guidelines(entries, &["read", "grep"], true).unwrap();
        assert_eq!(
            out,
            vec![
                "- read group head",
                "- grep caps at 16 KiB",
                "- read group tail",
                "- start wide",
            ]
        );
    }

    #[test]
    fn test_gate_any_of_renders_when_any_member_present() {
        // 变异：`AnyOf` 改成「全部成员都在场」⇒ 只剩 `write` 时不渲染 ⇒ 红。
        let block = GuidelineBlock::new(
            3,
            Gate::AnyOf(&["edit", "write"]),
            Scope::Always,
            lines(&["write with edit and write"]),
        );
        let out = expand_guidelines([("edit", &block)], &["write"], true).unwrap();
        assert_eq!(out, vec!["- write with edit and write"]);
    }

    #[test]
    fn test_gate_filters_block_when_absent() {
        // 单工具门控：归属工具不在场 ⇒ 不渲染；跨工具门控无成员在场 ⇒ 不渲染。
        let this_tool = GuidelineBlock::for_tool(1, lines(&["websearch only"]));
        let any_of = GuidelineBlock::new(
            2,
            Gate::AnyOf(&["read", "grep"]),
            Scope::Always,
            lines(&["read group"]),
        );

        let out = expand_guidelines(
            [("websearch", &this_tool), ("read", &any_of)],
            &["websearch"],
            true,
        )
        .unwrap();
        assert_eq!(out, vec!["- websearch only"]);
    }

    #[test]
    fn test_scope_main_only_skipped_for_subagent() {
        // `narrate = false`（探索子 agent）只去掉 MainOnly 那一块。
        // 变异：忽略 scope ⇒ 子 agent 多拿一条 ⇒ 红。
        let narrate = GuidelineBlock::new(
            8,
            Gate::AnyOf(&["read"]),
            Scope::MainOnly,
            lines(&["narrate progress"]),
        );
        let always = GuidelineBlock::for_tool(1, lines(&["always"]));
        let prompt = ToolPrompt::new("read", "r").with_guidelines(vec![always, narrate]);

        let entries: Vec<(&str, &GuidelineBlock)> = prompt.guideline_entries().collect();
        assert_eq!(
            expand_guidelines(entries.clone(), &["read"], false).unwrap(),
            vec!["- always"]
        );
        assert_eq!(
            expand_guidelines(entries, &["read"], true).unwrap(),
            vec!["- always", "- narrate progress"]
        );
    }

    #[test]
    fn test_expand_guidelines_duplicate_slot_fails() {
        let a = GuidelineBlock::for_tool(3, lines(&["a"]));
        let b = GuidelineBlock::for_tool(3, lines(&["b"]));
        let err =
            expand_guidelines([("read", &a), ("grep", &b)], &["read", "grep"], true).unwrap_err();
        assert!(matches!(&err, ToolError::InvalidInput(_)));
        let msg = err.to_string();
        assert!(msg.contains("slot"), "{msg}");
        assert!(msg.contains("read") && msg.contains("grep"), "{msg}");
    }

    #[test]
    fn test_sort_plugins_orders_by_order() {
        let plugins = vec![
            ToolPlugin::new(20, vec![tool("websearch")]),
            ToolPlugin::new(10, vec![tool("read")]),
        ];
        let sorted = sort_plugins(plugins).unwrap();
        let orders: Vec<u16> = sorted.iter().map(|p| p.order).collect();
        assert_eq!(orders, vec![10, 20]);
        assert_eq!(sorted[0].tools[0].spec().name, "read");
    }

    #[test]
    fn test_sort_plugins_duplicate_tool_name_fails() {
        // 跨插件重名必须 fail-fast，且错误信息点名两个插件位次 —— 否则第三方拿到
        // 裸 `duplicate tool name` 不知道是谁撞了谁。
        let plugins = vec![
            ToolPlugin::new(1, vec![tool("todo")]),
            ToolPlugin::new(2, vec![tool("read")]),
            ToolPlugin::new(3, vec![tool("todo")]),
        ];
        let err = sort_plugins(plugins).unwrap_err();
        assert!(matches!(&err, ToolError::InvalidInput(_)));
        let msg = err.to_string();
        assert!(msg.contains("duplicate tool name: todo"), "{msg}");
        assert!(msg.contains("plugin #1"), "{msg}");
        assert!(msg.contains("plugin #3"), "{msg}");
    }

    #[test]
    fn test_sort_plugins_duplicate_order_fails() {
        let plugins = vec![
            ToolPlugin::new(5, vec![tool("read")]),
            ToolPlugin::new(5, vec![tool("grep")]),
        ];
        let err = sort_plugins(plugins).unwrap_err();
        assert!(matches!(&err, ToolError::InvalidInput(_)));
        assert!(err.to_string().contains("duplicate plugin order: 5"));
    }

    #[test]
    fn test_sort_plugins_duplicate_slot_fails() {
        let plugins = vec![
            ToolPlugin::new(1, vec![]).with_prompts(vec![
                ToolPrompt::new("read", "r")
                    .with_guidelines(vec![GuidelineBlock::for_tool(4, lines(&["a"]))]),
            ]),
            ToolPlugin::new(2, vec![]).with_prompts(vec![
                ToolPrompt::new("grep", "g")
                    .with_guidelines(vec![GuidelineBlock::for_tool(4, lines(&["b"]))]),
            ]),
        ];
        let err = sort_plugins(plugins).unwrap_err();
        assert!(matches!(&err, ToolError::InvalidInput(_)));
        let msg = err.to_string();
        assert!(msg.contains("duplicate guideline slot: 4"), "{msg}");
    }
}
