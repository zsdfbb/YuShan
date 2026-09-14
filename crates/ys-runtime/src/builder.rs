use super::{Agent, BuildError};
use std::path::PathBuf;
use ys_component::RunLimits;
use ys_loop::{AgentLoop, BasicLoop};
use ys_tool::{ApprovalHandler, Tool, ToolRegistry};

/// Agent 的静态组合器。
///
/// **ADR-0010**：会话、事件出口与模型**不再**是 agent 的组成——它们归接线器，
/// 运行时经 [`AgentPorts`](crate::AgentPorts) 传入。故 builder 不含
/// `.session()` / `.events()` / `.model()`；轮边界控制源亦经端口传入，
/// 故也不含 `.cancel_token()`（`CancelToken` 已废，由 `Boundary::Abort` 取代）。
pub struct AgentBuilder {
    tools: Vec<Box<dyn Tool>>,
    loop_impl: Option<Box<dyn AgentLoop>>,
    limits: RunLimits,
    cwd: Option<PathBuf>,
    workspace_root: Option<PathBuf>,
    approval: Option<Box<dyn ApprovalHandler>>,
    system_prompt: Option<String>,
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            loop_impl: None,
            limits: RunLimits::default(),
            cwd: None,
            workspace_root: None,
            approval: None,
            system_prompt: None,
        }
    }

    pub fn tool(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.push(Box::new(tool));
        self
    }

    pub fn limits(mut self, limits: RunLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn working_dir(mut self, cwd: PathBuf, workspace_root: PathBuf) -> Self {
        self.cwd = Some(cwd);
        self.workspace_root = Some(workspace_root);
        self
    }

    pub fn approval(mut self, handler: impl ApprovalHandler + 'static) -> Self {
        self.approval = Some(Box::new(handler));
        self
    }

    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    pub fn build(self) -> Result<Agent, BuildError> {
        let registry =
            ToolRegistry::build(self.tools).map_err(|e| BuildError::ToolRegistry(e.to_string()))?;

        let loop_impl = self.loop_impl.unwrap_or_else(|| Box::new(BasicLoop));

        let cwd = self
            .cwd
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let workspace_root = self.workspace_root.unwrap_or_else(|| cwd.clone());

        Ok(Agent::new(
            loop_impl,
            registry,
            self.limits,
            cwd,
            workspace_root,
            self.approval,
            self.system_prompt,
        ))
    }
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self::new()
    }
}
