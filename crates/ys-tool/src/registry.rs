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
    /// 从一组 tool 构建 registry，并检查是否是重名
    ///
    /// `specs` 严格按**输入 `tools` 的顺序**收集 —— 不能由 `HashMap` 的迭代顺序决定，
    /// 否则顺序逐进程抖动（哈希随机种子），由它派生的提示词清单会让 prompt cache 前缀失效。
    pub fn build(tools: Vec<Box<dyn Tool>>) -> Result<Self, ToolError> {
        let mut map = HashMap::new();
        let mut specs = Vec::with_capacity(tools.len());
        for tool in tools {
            let spec = tool.spec();
            let name = spec.name.clone();
            if map.contains_key(&name) {
                return Err(ToolError::InvalidInput(format!(
                    "duplicate tool name: {name}"
                )));
            }
            map.insert(name, tool);
            specs.push(spec);
        }
        Ok(Self { tools: map, specs })
    }

    /// 按名字查找 tool
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    /// 获取全部 tool spec（构建时缓存）
    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    /// 获取全部 tool 名字（借用自缓存的 spec，零分配）。
    pub fn names(&self) -> Vec<&str> {
        self.specs.iter().map(|s| s.name.as_str()).collect()
    }

    /// 已注册 tool 的数量
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// 判断 registry 是否为空
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

    /// 按给定顺序构建 8 个**不同名**工具的 registry。
    fn build_ordered(order: &[&'static str; 8]) -> ToolRegistry {
        let tools: Vec<Box<dyn Tool>> = order
            .iter()
            .copied()
            .map(|n| Box::new(MockTool(n)) as Box<dyn Tool>)
            .collect();
        ToolRegistry::build(tools).unwrap()
    }

    #[test]
    fn test_specs_and_names_follow_input_order() {
        // N2 保序：`specs()` 必须严格按**输入顺序**，不能由 HashMap 迭代顺序决定
        //（哈希随机种子 ⇒ 顺序逐进程抖动 ⇒ 派生提示词清单让 prompt cache 前缀失效）。
        let order: [&'static str; 8] = ["a", "b", "c", "d", "e", "f", "g", "h"];
        let registry = build_ordered(&order);

        let spec_names: Vec<&str> = registry.specs().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(spec_names, order.to_vec(), "specs() 必须与输入同序");
        assert_eq!(registry.names(), order.to_vec(), "names() 必须与输入同序");
        assert_eq!(registry.len(), 8);

        // 变异：把 `build()` 改回「specs 由 map.values() 收集」→ 顺序随机，
        // 本测试以 1 - 1/8! = 40319/40320 的概率变红（漏检概率 ≈ 1/40320）。
    }

    #[test]
    fn test_specs_follow_input_order_reversed() {
        // 换一个输入顺序再验一次：钉住的是「跟随输入」，而不是「恰好等于某个固定序列」。
        let order: [&'static str; 8] = ["h", "g", "f", "e", "d", "c", "b", "a"];
        let registry = build_ordered(&order);

        assert_eq!(registry.names(), order.to_vec());

        // 变异：同 `test_specs_and_names_follow_input_order`（map.values()）→ 红。
    }
}
