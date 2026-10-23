//! Agent 装配工厂 —— 启动路径与会话内**换工作区**（`/resume`）共用同一份装配逻辑。
//!
//! # 为什么需要工厂
//!
//! 工作区在**运行期可变**：会话内 `/resume` 到另一条记录了别的工作树的会话时，当前
//! agent 必须按新工作区重建。一次重建要**同时**换掉三处派生自工作区的量：
//!
//! 1. **工具表** —— 每个工具在构造期捕获工作区（`ReadTool::new(ws)` 等，见
//!    `adapters/tools-basic`）；
//! 2. **`AgentBuilder::working_dir`** —— 相对路径的解析根；
//! 3. **系统提示词** —— `Current working directory: …` 与项目上下文
//!    （`<project_context>` 的 `AGENTS.md` / `YUSHAN.md`）。
//!
//! 三者必须一起换，否则会话处于不一致状态（工具写到旧树、提示词谎报 cwd）。启动路径
//! 与运行期重建若各写一份装配序列，必然漂移；本模块是那份序列的**唯一**出处。
//!
//! # 不变量：工厂无副作用
//!
//! [`sweep_fetched`](crate::sweep_fetched)（抓取落盘清理）与
//! [`SubagentsDir::sweep_old`](crate::subagent::SubagentsDir::sweep_old)（子产物清理）
//! 是**启动期一次性**动作，由 `main` 显式调用，**不在**本工厂里 —— 否则每次 `/resume`
//! 重建都会重跑清理（设计 §1.1 硬约束：装配本身无副作用）。
//!
//! # 不变量：工作区无关的量只进一次
//!
//! [`WebConfig`] / [`RunLimits`] / skill [`Catalogue`] / [`SubagentsDir`] / 子预算这些
//! **与工作区无关**的量在 [`AgentFactory::new`] 时冻结；[`AgentFactory::build`] 只把
//! **工作区**作为每次变化的入参。这与工具顺序、提示词工具清单、子表的「同一份真值」
//! 要求一致（见 `toolset` 模块头）。
//!
//! 后台命令上下文（[`BackgroundContext`]：进程级注册表 + 会话槽）同属**工作区无关**
//! 的量 ⇒ 经 [`AgentFactory::with_backgrounds`] 注入一次，随工厂冻结。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ys_component::RunLimits;
use ys_runtime::{Agent, BuildError};
use ys_tool::Tool;
use ys_tools_basic::{BashBgTool, BashTool};
use ys_tools_web::WebConfig;

use crate::prompt;
use crate::skills::Catalogue;
use crate::subagent::{SubagentSetup, SubagentTool, SubagentsDir};
use crate::wiring::BackgroundContext;
use crate::{fixed_prompt_tokens, toolset};

/// 装配工厂：持有工作区**无关**的不变量，按给定工作区产出 agent + 子派发配置。
pub struct AgentFactory {
    /// web 配置（工具表与子表共用；`Clone` 值，连接池共享）。
    web: WebConfig,
    /// skill 目录（进系统提示词的固定段；工作区无关 —— 扫描根取自启动 cwd，不随切换重扫）。
    catalogue: Catalogue,
    /// 父预算**基值**（`fixed_prompt_tokens` 每次按新提示词重算）。
    limits: RunLimits,
    /// 子 agent 的单回合 wall-clock 预算。
    child_budget: Duration,
    /// 子 agent 产物目录（`Arc` 跨重建共享；清理只在 `main` 启动期做）。
    subagents_dir: Arc<SubagentsDir>,
    /// 后台命令上下文（**进程级**）—— `None` = 本工厂不带后台能力。
    ///
    /// 注册表对象随工厂冻结 ⇒ `/resume` 重建 agent 时仍是**同一份**；
    /// 会话 id 在**共享槽**里（运行时读）⇒ 换会话不必重建工具表。
    backgrounds: Option<BackgroundContext>,
}

/// [`AgentFactory::build`] 的产物。
///
/// 把「agent 本身」与「退出后仍需的东西」分开装：`agent` 交给运行时；`subagent` 交给
/// `app_loop`（每回合造 runner）；`limits` / `hint` 供启动期提示用。
pub struct BuiltAgent {
    pub agent: Agent,
    pub subagent: SubagentSetup,
    /// 已填入 `fixed_prompt_tokens` 的最终预算（供 `fixed_cost_note` 判定）。
    pub limits: RunLimits,
    /// 工具表装配的降级提示（`rg`/`fd` 缺失等）—— **仅在启动期**消费。
    pub hint: Option<String>,
    /// `debug` 的 `run_start` 用：工具名 + JSON spec 字节数（顺序即注册顺序）。
    ///
    /// 与提示词工具清单同源 —— 注册顺序、提示词顺序、这里的元数据三者不可能漂移。
    pub tool_specs: Vec<(String, usize)>,
    /// 真实**父表**的 spec（装配产物）—— assembly 级断言用。
    ///
    /// 父表 = 基础 9 元 + `bash_bg` + `subagent` + `subagent_async`（后两者末位）。
    #[cfg(test)]
    pub parent_specs: Vec<ys_tool::ToolSpec>,
    /// 真实**子表**的 spec（装配产物）—— assembly 级断言用。
    ///
    /// 子表 = 基础 9 元（**无后台能力**）。
    #[cfg(test)]
    pub child_specs: Vec<ys_tool::ToolSpec>,
}

impl AgentFactory {
    /// 冻结工作区无关的量。`limits` 是**基值**（`fixed_prompt_tokens` 由 [`build`](Self::build) 按提示词填）。
    pub fn new(
        web: WebConfig,
        catalogue: Catalogue,
        limits: RunLimits,
        child_budget: Duration,
        subagents_dir: Arc<SubagentsDir>,
    ) -> Self {
        Self {
            web,
            catalogue,
            limits,
            child_budget,
            subagents_dir,
            backgrounds: None,
        }
    }

    /// 注入后台命令上下文（**进程级**）—— 随工厂冻结。
    ///
    /// `/resume` 重建 agent 时走的是**同一个工厂** ⇒ 注册表对象是**同一份**
    /// （后台命令跨工作区切换仍然可查）；会话 id 住在共享槽里 ⇒ 换会话不必重建工具表。
    ///
    /// 未注入时工厂**不带**后台能力（父表少两元）—— 测试装配走这条。
    pub fn with_backgrounds(mut self, backgrounds: BackgroundContext) -> Self {
        self.backgrounds = Some(backgrounds);
        self
    }

    /// 按 `workspace` 装配（工具表 / working_dir / system prompt 三者同源）。
    ///
    /// 顺序照设计 §1.2「启动期」：**先探测装工具**（`toolset` 是唯一装配接缝），
    /// **再由工具表派生提示词的工具清单** —— 同一份表，清单顺序 == 注册顺序。
    pub fn build(&self, workspace: &Path) -> Result<BuiltAgent, BuildError> {
        // 子 agent 的工具表 = 基础 9 元表（**去掉派发工具**）—— 结构上不可能递归。
        let (base_tools, hint) = toolset::build_tools(workspace, self.web.clone());
        let child_specs: Vec<_> = base_tools.iter().map(|t| t.spec()).collect();
        let child_prompt = prompt::build_system_prompt(workspace, &child_specs, &self.catalogue);

        // 子预算与主 agent 同源，但 `fixed_prompt_tokens` 必须填**子表**的值 ——
        // 否则子会话会系统性晚触发压缩。
        let mut child_limits = self.limits.clone();
        child_limits.fixed_prompt_tokens = fixed_prompt_tokens(&child_prompt, &child_specs);
        let subagent = SubagentSetup::shared(
            child_prompt,
            child_limits,
            self.child_budget,
            workspace.to_path_buf(),
            self.web.clone(),
            self.subagents_dir.clone(),
        );

        // 父表 = 基础 9 元 + `bash_bg` + 派发工具**追加末位**
        // （子 tools JSON 仍是父的字节前缀 ⇒ 命中缓存）。
        //
        // **插入位置是契约**：`bash_bg` 必须排在 `subagent` **之前** ⇒ 父表的前 9 项
        // 逐字节等于子表 ⇒ 父 JSON 仍以子 JSON 去末尾 `]` 开头。
        //
        // 后台命令的结果由注册表的清扫路径**主动投递**（`Boundary::Deferred`）——
        // 模型不需要轮询工具，故父表里没有 `bash_poll`。
        let mut tools: Vec<Box<dyn Tool>> = base_tools;
        if let Some(context) = &self.backgrounds {
            // **父表的 `bash` 换上挂了后台能力的版本**：超时不杀进程组，改为过继给
            // 注册表，结果由完成通知送达。位置不能动 —— 它仍在第 4 位，否则父表
            // 的前 9 项就不再逐字节等于子表（缓存前缀契约）。
            if let Some(bash) = tools.iter_mut().find(|t| t.spec().name == "bash") {
                *bash = Box::new(
                    BashTool::new(workspace.to_path_buf())
                        .with_backgrounds(context.tasks().clone(), context.session().clone()),
                );
            }
            tools.push(Box::new(BashBgTool::new(
                context.tasks().clone(),
                workspace.to_path_buf(),
                context.session().clone(),
            )));
        }
        // **追加顺序是契约**：`subagent_async` 必须排在 `subagent` **之后** ——
        // 子表（= 基础 9 元）仍是父表 JSON 的**字节前缀**（`toolset.rs` 的
        // `prefix_fingerprint_is_anchored_and_stable` 钉住这条）。
        tools.push(Box::new(SubagentTool::new(subagent.outer_timeout())));
        tools.push(Box::new(crate::subagent::SubagentAsyncTool::new()));
        let specs: Vec<_> = tools.iter().map(|t| t.spec()).collect();
        // `run_start` 的工具元数据：名字 + spec 的 JSON 字节数（不记内容）。
        let tool_specs: Vec<(String, usize)> = specs
            .iter()
            .map(|spec| {
                (
                    spec.name.clone(),
                    serde_json::to_string(spec)
                        .map(|json| json.len())
                        .unwrap_or(0),
                )
            })
            .collect();
        let system_prompt = prompt::build_system_prompt(workspace, &specs, &self.catalogue);

        // **压缩耦合修正**（设计 §选择理由 E）：量一次每轮固定开销交给 `BasicLoop`。
        let mut limits = self.limits.clone();
        limits.fixed_prompt_tokens = fixed_prompt_tokens(&system_prompt, &specs);

        let agent = crate::build_agent(
            system_prompt,
            workspace.to_path_buf(),
            tools,
            limits.clone(),
        )?;
        Ok(BuiltAgent {
            agent,
            subagent,
            limits,
            hint,
            tool_specs,
            #[cfg(test)]
            parent_specs: specs,
            #[cfg(test)]
            child_specs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ys_tools_basic::{BackgroundTasks, BgLimits, SessionSlot};
    use ys_tools_web::FetchedDir;

    use crate::subagent::SubagentsDir;

    /// 进程内唯一临时目录（原子序号 + `process::id()`，并行安全）。
    fn unique_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "yushan_assembly_{tag}_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 生产同形的工厂 —— **带**后台命令上下文（注册表 + 会话槽）。
    ///
    /// 注册表落在临时目录里（**绝不**碰真实的 `YUSHAN_BG_DIR`）。
    fn factory(base: &Path) -> AgentFactory {
        // 确定性 web 配置：绝不 `from_env()`（结果随机器 env 漂移）。
        let web = WebConfig::offline(FetchedDir::Temp(base.join("fetched")));
        let catalogue = crate::skills::catalogue(Vec::new(), crate::skills::Budget::default());
        let backgrounds = BackgroundContext::new(
            Arc::new(BackgroundTasks::new(base.join("bg"), BgLimits::default())),
            SessionSlot::new("assembly-test"),
        );
        AgentFactory::new(
            web,
            catalogue,
            RunLimits::default(),
            Duration::from_secs(30),
            Arc::new(SubagentsDir::at(base.join("subagents"), false)),
        )
        .with_backgrounds(backgrounds)
    }

    /// 同一工厂、两个工作区：工具表恒 12 元（9 基础 + bg×2 + subagent），而 system prompt 的
    /// `Current working directory` **跟着工作区变** —— 这是「三者一起换」中提示词那一处
    /// 的可观测证据。
    ///
    /// 观测手段是 `limits.fixed_prompt_tokens`：它是 system prompt + 工具表 spec 的估算
    /// token 数。两个工作区的目录名**长度不同**（`a` vs `a-longer-name`）⇒ 提示词里
    /// `Current working directory: …` 那一行长度不同 ⇒ 估算值不同。
    ///
    /// 变异：把 `build` 里的 `build_system_prompt(workspace, …)` 换成常量 cwd（或漏传
    /// 工作区）⇒ 两条提示词的 cwd 行**逐字相同** ⇒ 估算值相等 ⇒ 第一条 `assert_ne!` 变红。
    #[test]
    fn build_tracks_workspace_in_prompt_fixed_cost() {
        let base = unique_dir("tracks");
        let short = base.join("a");
        let long = base.join("a-much-longer-workspace-name");
        std::fs::create_dir_all(&short).unwrap();
        std::fs::create_dir_all(&long).unwrap();

        let f = factory(&base);
        let built_short = f.build(&short).unwrap();
        let built_long = f.build(&long).unwrap();

        let expected: Vec<String> = crate::toolset::EXPECTED_ORDER
            .iter()
            .map(|s| s.to_string())
            .chain([
                "bash_bg".to_string(),
                "subagent".to_string(),
                "subagent_async".to_string(),
            ])
            .collect();
        assert_eq!(
            built_short.agent.tool_names(),
            expected,
            "父表应为 9 基础 + bash_bg + subagent + subagent_async（后两者追加在末位）"
        );
        assert_eq!(
            built_long.agent.tool_names(),
            expected,
            "换工作区不得改变工具表构成"
        );

        assert_eq!(
            built_short.limits.context_window, built_long.limits.context_window,
            "窗口与工作区无关（同一基值）"
        );
        assert_ne!(
            built_short.limits.fixed_prompt_tokens, built_long.limits.fixed_prompt_tokens,
            "system prompt 必须随工作区变（cwd 行长度不同 ⇒ 固定开销不同）"
        );

        // 同一工作区两次构建 ⇒ 固定开销逐值相同（提示词派生是纯函数，无随机/时间量）。
        let again = f.build(&short).unwrap();
        assert_eq!(
            again.limits.fixed_prompt_tokens, built_short.limits.fixed_prompt_tokens,
            "同一工作区两次构建的固定开销必须一致"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// **assembly 级断言** —— 真实父表 12 元 / 子表 9 元；父表 JSON 以子表 JSON
    /// 去末尾 `]` 开头；派发工具族收尾。
    ///
    /// **为什么必须是 assembly 级**：`toolset` 里那条 9 元顺序用例**自拼父表**
    /// （自己在子表后面追加 `subagent`）⇒ 往父表里插新工具后它照样全绿，
    /// 却已经不再描述真实父表。本用例断言的是**装配产物本身**（`build` 的真实输出），
    /// 不是另拼一份。
    ///
    /// 变异：把 `bash_bg` 插到 `subagent` **之后** ⇒ 本用例的追加段断言与
    /// `real_parent_table_ends_with_the_subagent_tool` 的末位断言**同时**变红。
    #[test]
    fn real_parent_table_keeps_the_child_table_as_its_byte_prefix() {
        let base = unique_dir("parent_prefix");
        let f = factory(&base);
        let built = f.build(&base).unwrap();

        // ① 真实父表 12 元、子表 9 元（子 agent 天然没有后台能力）。
        let parent_names: Vec<&str> = built
            .parent_specs
            .iter()
            .map(|spec| spec.name.as_str())
            .collect();
        assert_eq!(parent_names.len(), 12, "真实父表应为 12 元");
        assert_eq!(built.child_specs.len(), 9, "真实子表应为 9 元");
        assert_eq!(
            built.agent.tool_names().len(),
            12,
            "注册进 agent 的父表应为 12 元"
        );
        assert!(
            !built.child_specs.iter().any(|spec| spec.name == "bash_bg"),
            "子表不得含后台工具（子 agent 没有后台能力）"
        );

        // ② 真实父表 JSON 以子表 JSON 去末尾 `]` 开头（前缀不变量）。
        let parent_json = serde_json::to_string(&built.parent_specs).unwrap();
        let child_json = serde_json::to_string(&built.child_specs).unwrap();
        assert!(
            parent_json.starts_with(&child_json[..child_json.len() - 1]),
            "父表 JSON 必须以子表 JSON 去末尾 `]` 开头（前缀不变量）"
        );

        // 追加段（子表之后的那几项）必须是
        // `bash_bg` + `subagent` + `subagent_async`
        // —— 顺序即契约：后台工具排在派发工具**之前**；`subagent_async`
        // 排在 `subagent` **之后**（新增项只能往后加，否则已缓存的父前缀提前断掉）。
        let appended = &parent_json[child_json.len() - 1..];
        let tail: Vec<ys_tool::ToolSpec> = built.parent_specs[built.child_specs.len()..].to_vec();
        assert_eq!(
            tail.iter()
                .map(|spec| spec.name.as_str())
                .collect::<Vec<&str>>(),
            vec!["bash_bg", "subagent", "subagent_async"],
            "父表的追加段必须是 bash_bg + subagent + subagent_async（顺序即契约）"
        );
        let tail_json = serde_json::to_string(&tail).unwrap();
        assert_eq!(
            appended,
            format!(",{}", &tail_json[1..]),
            "父表 JSON 的追加段必须逐字节等于 bash_bg + 派发工具族的序列化"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// **assembly 级断言（末位）** —— 派发工具族收尾。
    ///
    /// 派发工具永远追加在最后 ⇒ 子表（基础 9 元）既是父表的**前缀**，又与末位相距
    /// 两项（`bash_bg` / `subagent`）。
    ///
    /// 变异：把 `bash_bg` 插到 `subagent` **之后** ⇒ 本用例变红。
    #[test]
    fn real_parent_table_ends_with_the_subagent_tool() {
        let base = unique_dir("parent_last");
        let f = factory(&base);
        let built = f.build(&base).unwrap();

        let parent_names: Vec<&str> = built
            .parent_specs
            .iter()
            .map(|spec| spec.name.as_str())
            .collect();
        // **派发工具族必须在末位**（顺序固定）——子表（基础 9 元）仍是父表 JSON 的
        // 字节前缀。`subagent_async` 必须排在 `subagent` **之后**，不得插在中间
        // （那会让已缓存的父前缀提前断掉）。
        assert_eq!(
            &parent_names[parent_names.len() - 2..],
            &["subagent", "subagent_async"],
            "派发工具族必须按 [subagent, subagent_async] 收尾"
        );
        assert_eq!(
            built.agent.tool_names().last().map(String::as_str),
            Some("subagent_async"),
            "注册进 agent 的父表末位也必须是 `subagent_async`"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
