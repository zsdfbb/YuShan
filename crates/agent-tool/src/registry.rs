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

    /// Get all tool names (borrowed from cached specs, zero allocation).
    pub fn names(&self) -> Vec<&str> {
        self.specs.iter().map(|s| s.name.as_str()).collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Tool, ToolContext, ToolResult, ToolSpec};

    struct MockTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for MockTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(self.0, "desc", serde_json::json!({}))
        }

        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            unimplemented!()
        }
    }

    #[test]
    fn test_names_returns_all_tool_names() {
        let registry = ToolRegistry::build(vec![
            Box::new(MockTool("read")),
            Box::new(MockTool("write")),
        ])
        .unwrap();
        let names = registry.names();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"read"));
        assert!(names.contains(&"write"));
    }

    #[test]
    fn test_names_empty_registry() {
        let registry = ToolRegistry::build(vec![]).unwrap();
        assert!(registry.names().is_empty());
    }
}
