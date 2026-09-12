use super::{Agent, BuildError};
use std::path::PathBuf;
use ys_component::RunLimits;
use ys_core::CancelToken;
use ys_event::EventSink;
use ys_loop::{AgentLoop, BasicLoop};
use ys_model::Model;
use ys_session::Session;
use ys_tool::{ApprovalHandler, Tool, ToolRegistry};

pub struct AgentBuilder {
    model: Option<Box<dyn Model>>,
    tools: Vec<Box<dyn Tool>>,
    session: Option<Box<dyn Session>>,
    events: Option<Box<dyn EventSink>>,
    loop_impl: Option<Box<dyn AgentLoop>>,
    cancel: CancelToken,
    limits: RunLimits,
    cwd: Option<PathBuf>,
    workspace_root: Option<PathBuf>,
    approval: Option<Box<dyn ApprovalHandler>>,
    system_prompt: Option<String>,
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self {
            model: None,
            tools: Vec::new(),
            session: None,
            events: None,
            loop_impl: None,
            cancel: CancelToken::new(),
            limits: RunLimits::default(),
            cwd: None,
            workspace_root: None,
            approval: None,
            system_prompt: None,
        }
    }

    pub fn model(mut self, model: impl Model + 'static) -> Self {
        self.model = Some(Box::new(model));
        self
    }

    pub fn tool(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.push(Box::new(tool));
        self
    }

    pub fn session(mut self, session: impl Session + 'static) -> Self {
        self.session = Some(Box::new(session));
        self
    }

    pub fn events(mut self, events: impl EventSink + 'static) -> Self {
        self.events = Some(Box::new(events));
        self
    }

    pub fn cancel_token(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
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
        let session = self.session.ok_or(BuildError::MissingSession)?;
        let events = self.events.ok_or(BuildError::MissingEvents)?;

        let registry =
            ToolRegistry::build(self.tools).map_err(|e| BuildError::ToolRegistry(e.to_string()))?;

        let loop_impl = self.loop_impl.unwrap_or_else(|| Box::new(BasicLoop));

        let cwd = self
            .cwd
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let workspace_root = self.workspace_root.unwrap_or_else(|| cwd.clone());

        Ok(Agent::new(
            loop_impl,
            self.model,
            registry,
            session,
            events,
            self.cancel,
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
