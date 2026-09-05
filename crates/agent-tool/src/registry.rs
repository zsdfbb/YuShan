use std::collections::HashMap;

use super::{Tool, ToolError, ToolSpec};

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
    specs: Vec<ToolSpec>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tool_names", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ToolRegistry {
    /// Build a registry from a list of tools, checking for duplicate names
    pub fn build(tools: Vec<Box<dyn Tool>>) -> Result<Self, ToolError> {
        let mut map = HashMap::new();
        for tool in tools {
            let spec = tool.spec();
            if map.contains_key(&spec.name) {
                return Err(ToolError::InvalidInput(format!(
                    "duplicate tool name: {}",
                    spec.name
                )));
            }
            map.insert(spec.name, tool);
        }
        let specs: Vec<ToolSpec> = map.values().map(|t| t.spec()).collect();
        Ok(Self { tools: map, specs })
    }

    /// Lookup tool by name
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    /// Get all tool specs (cached at build time)
    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    /// Number of registered tools
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Check if registry is empty
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
