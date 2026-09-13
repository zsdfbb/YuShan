#[derive(Debug, Clone)]
pub struct ProviderCompat {
    /// DeepSeek: reasoning_content 字段
    pub has_reasoning_content: bool,
    /// MiniMax: tool_calls 可能作为 text 返回
    pub tool_calls_as_text: bool,
    /// 是否在流式请求里发送 `stream_options.include_usage` 以获取 token 统计。
    ///
    /// 默认开启（见 [`ProviderCompat::standard`]）。OpenAI 兼容端点普遍容忍
    /// 未知顶层字段；少数严格校验的网关若不接受该字段会返回 400，此时
    /// `OpenAICompatibleModel::complete` 会剥掉该字段一次性重试（见 lib.rs），
    /// 保证「安全」与「token 统计完整」兼顾。仅当已知某 provider 一定拒绝时
    /// （如未经验证的 MiniMax 兼容层）才显式关闭。
    pub supports_stream_usage: bool,
}

/// 手写而非 derive：`supports_stream_usage` 的「默认」必须是 **true**。
/// 若沿用 derive 的 `false`，任何 `ProviderCompat { ..Default::default() }`
/// 构造都会静默丢掉 token 统计（正是本次修复的回归点），
/// 因此让 `Default` 与 [`ProviderCompat::standard`] 语义一致。
impl Default for ProviderCompat {
    fn default() -> Self {
        Self {
            has_reasoning_content: false,
            tool_calls_as_text: false,
            supports_stream_usage: true,
        }
    }
}

impl ProviderCompat {
    pub fn deepseek() -> Self {
        Self {
            has_reasoning_content: true,
            // DeepSeek 兼容 OpenAI 的 stream_options.include_usage，流式末包带 usage。
            supports_stream_usage: true,
            ..Default::default()
        }
    }

    pub fn minimax() -> Self {
        Self {
            tool_calls_as_text: true,
            // MiniMax 兼容层对未知顶层字段是否放行未经验证，保守关闭：
            // 宁可 token 统计为 0，也不冒 400 的风险。（若实测放行，可改为 true。）
            supports_stream_usage: false,
            ..Default::default()
        }
    }

    /// 标准 / 未知端点：默认**开启**流式 usage，以拿到 token 统计。
    ///
    /// OpenAI 兼容端点普遍容忍未知顶层字段（反序列化到 struct 时忽略未知键），
    /// 故默认开启；若某严格校验端点因此返回 400，
    /// `OpenAICompatibleModel::complete` 会剥掉 `stream_options` 一次性重试，
    /// 不会让整个请求失败。需要显式关闭时：
    /// `ProviderCompat { supports_stream_usage: false, ..ProviderCompat::standard() }`。
    pub fn standard() -> Self {
        Self {
            supports_stream_usage: true,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `standard()` 默认开启流式 usage——env-var 配置的用户落到 custom/standard，
    /// 若默认关闭则 token 统计恒为 0（本次修复的回归点）。
    #[test]
    fn standard_enables_stream_usage() {
        assert!(ProviderCompat::standard().supports_stream_usage);
        assert!(ProviderCompat::default().supports_stream_usage);
    }

    /// deepseek 显式开启；minimax 保守关闭。
    #[test]
    fn provider_specific_stream_usage_flags() {
        assert!(ProviderCompat::deepseek().supports_stream_usage);
        assert!(!ProviderCompat::minimax().supports_stream_usage);
        assert!(ProviderCompat::minimax().tool_calls_as_text);
        assert!(ProviderCompat::deepseek().has_reasoning_content);
    }
}
