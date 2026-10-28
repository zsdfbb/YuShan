/// `reasoning_effort` 的 wire 方言。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EffortDialect {
    /// 原样透传配置值；是否发送只由 `supports_reasoning_effort` 决定。
    #[default]
    Verbatim,
    /// GLM：按模型代际折叠到该代际真正接受的档位集合；`off` 永不原样发出。
    Glm,
    /// MiMo（小米）：按该端点实测接受的档位集合折叠。
    ///
    /// 实测合法集为 `none|low|medium|high|max` —— `minimal` / `xhigh` / `off`
    /// 直接返回 400（不静默折叠），故本地必须先映射。
    Mimo,
}

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
    /// 是否接受顶层 `reasoning_effort` 字段。默认关闭：未知兼容端点不应收到
    /// 它们可能严格校验的扩展字段。
    pub supports_reasoning_effort: bool,
    /// `reasoning_effort` 的取值方言：见 [`EffortDialect`]。
    pub effort_dialect: EffortDialect,
    /// 是否接受顶层 `thinking: {"type": "enabled"|"disabled"}` 开关。
    ///
    /// 默认关闭：未知兼容端点不应收到它们可能严格校验的扩展字段。
    /// 目前只有 DeepSeek 方言**实测确认**接受该字段（见
    /// [`ProviderCompat::deepseek`]）；GLM 明确「始终思考，不支持关闭」。
    pub supports_thinking_control: bool,
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
            supports_reasoning_effort: false,
            effort_dialect: EffortDialect::Verbatim,
            supports_thinking_control: false,
        }
    }
}

impl ProviderCompat {
    pub fn deepseek() -> Self {
        Self {
            has_reasoning_content: true,
            // DeepSeek 兼容 OpenAI 的 stream_options.include_usage，流式末包带 usage。
            supports_stream_usage: true,
            // 思考控制的权威口径见 <https://api-docs.deepseek.com/zh-cn/guides/thinking_mode>：
            // `thinking: {"type": "enabled"|"disabled"}` 是**开关**，
            // `reasoning_effort` 是**强度**，两者独立。
            supports_thinking_control: true,
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

    /// OpenCode Go 网关（`https://opencode.ai/zen/go/v1`）。
    ///
    /// 该网关上的**推理型模型按 DeepSeek 方言**回思考内容：实测
    /// `deepseek-v4-pro` / `deepseek-flash` 的流式 delta 与终包都带
    /// `reasoning_content`（非标准 `reasoning` 字段）。故解析开关沿用
    /// [`ProviderCompat::deepseek`]。
    ///
    /// 网关实测接受顶层 `reasoning_effort`，故显式声明支持。
    /// `supports_stream_usage` 保持开启：网关放行 `stream_options.include_usage`；
    /// 若将来拒绝，适配器的「剥参数一次性回退」仍能兜住。
    ///
    /// **思考开关**：本仓的第一优选模型 `deepseek-*` 经本网关接入，而它是否真能
    /// 关闭思考一度无人知道。实测（各 n=3）：短问答带
    /// `thinking:{"type":"disabled"}` 时 `reasoning_tokens` **0 / 0 / 0**、
    /// 不发送时 49 / 54 / 42；需推理任务（「说出 3 个 Rust 所有权规则」）为
    /// **0 / 0 / 0** 对 **229 / 98 / 103**（合计 430）。
    /// ⇒ **字段不被拒且开关真的生效**，故置位。
    ///
    /// ⚠️ 置位**不等于**发送：请求体里是否带 `thinking` 仍由 `config.disable_thinking`
    /// 与 `request.disable_thinking` 共同决定（两者默认都是「不干预」）⇒
    /// **不配 `/effort off` 时请求体逐字节不变**。
    /// 同网关的 `minimax-*` 是否接受该字段**未测** ⇒ 本能力位不得据此扩散到别的方言。
    pub fn opencode_go() -> Self {
        Self {
            has_reasoning_content: true,
            supports_reasoning_effort: true,
            supports_thinking_control: true,
            ..Default::default()
        }
    }

    /// 智谱 GLM（bigmodel）。
    ///
    /// 方言：思考增量走 `reasoning_content`（同 DeepSeek）；接受顶层
    /// `reasoning_effort`，但可用档位随模型代际收缩（见 [`EffortDialect::Glm`]）：
    /// 实测 `glm-5.3` 只接受 `low` / `high` / `max`（`minimal` / `medium` / `xhigh`
    /// 返回 code 1210「该模型始终思考，不支持关闭思考」），`glm-5.2` 及更早接受全集。
    pub fn glm() -> Self {
        Self {
            has_reasoning_content: true,
            supports_reasoning_effort: true,
            effort_dialect: EffortDialect::Glm,
            ..Default::default()
        }
    }

    /// 小米 MiMo（`api.xiaomimimo.com`）。
    ///
    /// 方言：思考增量走 `reasoning_content`（同 DeepSeek）；`tool_calls` 在
    /// `tool_calls` 字段里（不是 text）；流式末包带 usage。
    ///
    /// 接受顶层 `reasoning_effort`，但可用档位是该端点自己的子集
    /// （见 [`EffortDialect::Mimo`]）。
    pub fn mimo() -> Self {
        Self {
            has_reasoning_content: true,
            supports_reasoning_effort: true,
            effort_dialect: EffortDialect::Mimo,
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

    /// 该 **模型** 是否接受 `reasoning_effort`。
    ///
    /// GLM 方言的判据是**能否把 id 识别为 GLM 模型**，而不是代际是否够新：
    /// 实测 5.2 及更早代际与 5.3 一样接受该字段，只是各自接受的档位集合不同
    /// （折叠见 [`ProviderCompat::effort_for`]）。解析不出代际的 id 一律
    /// fail-closed（不发）——非 GLM 的模型收到该字段可能返回 400。
    pub fn supports_effort(&self, model: &str) -> bool {
        if !self.supports_reasoning_effort {
            return false;
        }
        match self.effort_dialect {
            // 原样透传的方言没有模型级信息，信任能力位。
            EffortDialect::Verbatim => true,
            EffortDialect::Glm => glm_generation(model).is_some(),
            EffortDialect::Mimo => is_mimo_chat_model(model),
        }
    }

    /// 这次请求实际要发的 `reasoning_effort`（`None` = 整个字段不序列化）。
    ///
    /// `Verbatim` 方言原样返回配置值（`None` 仍为 `None`）。
    ///
    /// `Glm` 方言先过模型级门控（[`ProviderCompat::supports_effort`]），再按代际
    /// 折叠到该代际真正接受的档位集合。**`off` 永不原样发出**：它不是 GLM 的合法
    /// wire 值（合法集为 `none|minimal|low|medium|high|xhigh|max`），原样发返回
    /// code 1210；任何代际都必须先把它映射成一个合法档位。
    pub fn effort_for(&self, model: &str, configured: Option<&str>) -> Option<String> {
        let configured = configured?;
        // 门控只在这里判定一次：`supports_effort` 是唯一的模型级判断来源。
        if !self.supports_effort(model) {
            return None;
        }
        match self.effort_dialect {
            EffortDialect::Verbatim => Some(configured.to_string()),
            EffortDialect::Glm => {
                let generation = glm_generation(model)?;
                // 白名单之外的取值来自上游配置错误，不猜映射，直接不发。
                match generation {
                    g if g >= (5, 3) => match configured {
                        "off" | "minimal" | "low" => Some("low".into()),
                        "medium" | "high" => Some("high".into()),
                        "xhigh" | "max" => Some("max".into()),
                        _ => None,
                    },
                    (5, 2) => match configured {
                        "off" | "minimal" => Some("minimal".into()),
                        "low" | "medium" | "high" => Some("high".into()),
                        "xhigh" | "max" => Some("max".into()),
                        _ => None,
                    },
                    // 5.2 之前的代际：实测接受全集（含 `xhigh` / `max`），原样透传。
                    // 唯一例外是 `off`——非法 wire 值，映射到 `minimal`（放弃思考）。
                    // 本仓的种子只含 5.3 家族，故这条分支仅在用户手敲一个更早的 id 时到达。
                    _ => match configured {
                        "off" => Some("minimal".into()),
                        "minimal" | "low" | "medium" | "high" | "xhigh" | "max" => {
                            Some(configured.to_string())
                        }
                        _ => None,
                    },
                }
            }
            EffortDialect::Mimo => {
                // 白名单之外的取值来自上游配置错误，不猜映射，直接不发。
                match configured {
                    "off" | "minimal" => Some("low".into()),
                    "low" => Some("low".into()),
                    "medium" => Some("medium".into()),
                    "high" => Some("high".into()),
                    "xhigh" | "max" => Some("max".into()),
                    _ => None,
                }
            }
        }
    }
}

/// MiMo 音频模型的**能力标记**（id 的 `-` 分段里出现即判为音频）。
///
/// 该端点的目录把音频模型排在**最前**（实测 `/v1/models` 首条是 `mimo-v2.5-asr`），
/// 且它们在 `/chat/completions` 上**必然 400**：
///
/// - `mimo-v2.5-asr` → `ASR request requires a user message with input_audio content`
/// - `mimo-v2.5-tts` / `-tts-voiceclone` / `-tts-voicedesign`
///   → `messages must contain an assistant role for TTS model`
///
/// 故音频 id 既不进 `/model` 候选，也不该收到 `reasoning_effort`。
pub const MIMO_AUDIO_TOKENS: &[&str] = &["asr", "tts"];

/// MiMo **不用于本仓**的模型标记（id 的 `-` 分段里出现即排除）。
///
/// `ultraspeed` 是**用户明确禁用**的模型：它不进 `/model` 候选，
/// 也不该收到 `reasoning_effort`。
pub const MIMO_DISABLED_TOKENS: &[&str] = &["ultraspeed"];

/// MiMo **不进目录**的模型标记 —— [`MIMO_AUDIO_TOKENS`] 与
/// [`MIMO_DISABLED_TOKENS`] 的**并集**。
///
/// 目录过滤用这一份，模型级门控用那两份；两处必须覆盖**同一批**模型
/// （目录里放行了一个、门控却不认它 ⇒ 用户选中就 400）。一致性由
/// `mimo_excluded_tokens_cover_audio_and_disabled` 钉住。
pub const MIMO_EXCLUDED_TOKENS: &[&str] = &["asr", "tts", "ultraspeed"];

/// id 的 `-` 分段里是否命中任一 token（大小写不敏感）。
///
/// **按分段精确匹配**，而非子串包含：子串匹配会被无关的连续字母误伤
/// （如 `mimo-v2.6-classify` 里的 `asr`）。
pub fn id_has_token(id: &str, tokens: &[&str]) -> bool {
    id.split('-')
        .any(|segment| tokens.iter().any(|t| segment.eq_ignore_ascii_case(t)))
}

/// 该 id 是否是 MiMo 的**音频**模型（非 chat 模型）。
///
/// 大小写不敏感。分段里出现 [`MIMO_AUDIO_TOKENS`] 任一项即判为音频。
pub fn is_mimo_audio_model(model: &str) -> bool {
    id_has_token(model, MIMO_AUDIO_TOKENS)
}

/// 该 id 是否是 MiMo **不用于本仓**的模型（用户明确禁用）。
///
/// 大小写不敏感。分段里出现 [`MIMO_DISABLED_TOKENS`] 任一项即命中。
pub fn is_mimo_disabled_model(model: &str) -> bool {
    id_has_token(model, MIMO_DISABLED_TOKENS)
}

/// 该 id 是否是 MiMo 的 **chat** 模型（可接受 `reasoning_effort`）。
///
/// 判据：id 以 `mimo-` 开头**且**不是音频模型**且**不是用户禁用的模型。
///
/// **识别不出 MiMo 模型 ⇒ `false`（fail-closed）**：与 GLM 方言同一原则 ——
/// 「解析不出身份就不发」，避免把字段发给不接受它的端点。
pub fn is_mimo_chat_model(model: &str) -> bool {
    let Some(rest) = model
        .get(..5)
        .filter(|prefix| prefix.eq_ignore_ascii_case("mimo-"))
        .and_then(|_| model.get(5..))
    else {
        return false;
    };
    // 前缀之后必须有**实际的模型名**：`mimo-`（光秃秃的前缀）不是模型。
    !rest.is_empty() && !is_mimo_audio_model(model) && !is_mimo_disabled_model(model)
}

/// 从模型 id 解析 GLM 代际 `(major, minor)`，无法解析时 `None`。
///
/// 大小写不敏感；`glm-` 之后取 `<major>`，紧跟 `.` 时再取 `<minor>`，minor 后可跟任意字符
/// （`glm-5.3-flash` / `glm-5.2v` 均解析为 `(5, 3)` / `(5, 2)`）。
/// 省略 minor 的 id（`glm-5`）按 `(5, 0)` 处理，以便与显式的 `glm-5.0` 同一分支。
/// 缺前缀、缺 major，或 `.` 后没有数字 ⇒ `None`，由调用方 fail-closed。
fn glm_generation(model: &str) -> Option<(u32, u32)> {
    let rest = model
        .get(..4)
        .filter(|prefix| prefix.eq_ignore_ascii_case("glm-"))
        .and_then(|_| model.get(4..))?;

    let (major, after_major) = read_digits(rest)?;
    match after_major.strip_prefix('.') {
        Some(after_dot) => {
            let (minor, _) = read_digits(after_dot)?;
            Some((major, minor))
        }
        None => Some((major, 0)),
    }
}

/// 消费前导的连续 ASCII 数字，返回 `(数值, 剩余部分)`；无数字时 `None`。
fn read_digits(text: &str) -> Option<(u32, &str)> {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    if end == 0 {
        return None;
    }
    Some((text[..end].parse().ok()?, &text[end..]))
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

    /// opencode go 的推理模型在**响应侧**用 `reasoning_content`（同 DeepSeek 的
    /// 解析形状），且放行 `stream_options.include_usage` 与顶层 `reasoning_effort`。
    #[test]
    fn opencode_go_parses_deepseek_reasoning_content_and_stream_usage() {
        let compat = ProviderCompat::opencode_go();
        assert!(compat.has_reasoning_content);
        assert!(compat.supports_stream_usage);
        assert!(compat.supports_reasoning_effort);
        assert!(!compat.tool_calls_as_text);
    }

    /// opencode go 声明**思考开关**能力位（第一优选模型 `deepseek-*` 经它接入）。
    ///
    /// 实测依据（直打网关，各 n=3）：短问答 `thinking:{type:disabled}` ⇒
    /// `reasoning_tokens` 0/0/0 vs 不发送 49/54/42；需推理任务 0/0/0 vs 229/98/103。
    /// 见 `docs/arch/effort-per-call-type/cost-analysis.md` §6。
    ///
    /// **反向断言同样承重**：不能因为「一家能用」就把它扩散到别的方言 ——
    /// GLM 明确「始终思考」，standard / minimax 未测。
    ///
    /// 变异：把 `supports_thinking_control: true` 从 `opencode_go()` 删掉 ⇒ 第一条变红。
    #[test]
    fn opencode_go_and_deepseek_are_the_only_thinking_control_vendors() {
        assert!(ProviderCompat::opencode_go().supports_thinking_control);
        assert!(ProviderCompat::deepseek().supports_thinking_control);

        // 不向后扩散：这些方言没有证据。
        for compat in [
            ProviderCompat::standard(),
            ProviderCompat::default(),
            ProviderCompat::glm(),
            ProviderCompat::mimo(),
            ProviderCompat::minimax(),
        ] {
            assert!(
                !compat.supports_thinking_control,
                "未实测的方言不得声明思考开关能力位"
            );
        }
    }

    /// 只有明确声明支持的 provider 才允许发送 `reasoning_effort`。
    ///
    /// 声明支持 ≠ 每个模型都发送：模型级门控在 [`ProviderCompat::supports_effort`]。
    #[test]
    fn constructors_pin_reasoning_effort_support() {
        assert!(!ProviderCompat::default().supports_reasoning_effort);
        assert!(!ProviderCompat::standard().supports_reasoning_effort);
        assert!(!ProviderCompat::deepseek().supports_reasoning_effort);
        assert!(!ProviderCompat::minimax().supports_reasoning_effort);
        assert!(ProviderCompat::opencode_go().supports_reasoning_effort);
        assert!(ProviderCompat::glm().supports_reasoning_effort);
        assert!(ProviderCompat::mimo().supports_reasoning_effort);
    }

    /// GLM 的能力位：思考走 `reasoning_content`，流式末包带 usage，
    /// 接受顶层 `reasoning_effort`；tool_calls 仍在 `tool_calls` 里（不是 text）。
    #[test]
    fn glm_capabilities() {
        let compat = ProviderCompat::glm();
        assert!(compat.has_reasoning_content);
        assert!(compat.supports_stream_usage);
        assert!(compat.supports_reasoning_effort);
        assert!(!compat.tool_calls_as_text);
    }

    /// `effort_dialect` 的默认值固定为 `Verbatim`：
    /// 把 `Glm` 设成默认值会让未声明方言的 provider 静默收到折叠后的档位。
    #[test]
    fn effort_dialect_defaults_to_verbatim() {
        assert_eq!(
            ProviderCompat::standard().effort_dialect,
            EffortDialect::Verbatim
        );
        assert_eq!(
            ProviderCompat::default().effort_dialect,
            EffortDialect::Verbatim
        );
        assert_eq!(
            ProviderCompat::deepseek().effort_dialect,
            EffortDialect::Verbatim
        );
        assert_eq!(
            ProviderCompat::minimax().effort_dialect,
            EffortDialect::Verbatim
        );
        assert_eq!(
            ProviderCompat::opencode_go().effort_dialect,
            EffortDialect::Verbatim
        );
        assert_eq!(ProviderCompat::glm().effort_dialect, EffortDialect::Glm);
        assert_eq!(ProviderCompat::mimo().effort_dialect, EffortDialect::Mimo);
    }

    /// `Verbatim` 方言逐字透传：不 trim、不改写，`None` 仍为 `None`。
    #[test]
    fn verbatim_passes_configured_value_through() {
        let compat = ProviderCompat::opencode_go();
        assert_eq!(
            compat.effort_for("anything", Some("xhigh")),
            Some("xhigh".into())
        );
        assert_eq!(
            compat.effort_for("anything", Some(" high ")),
            Some(" high ".into())
        );
        assert_eq!(compat.effort_for("anything", None), None);
        assert!(compat.supports_effort("anything"));

        // 未声明支持的 provider 无论模型为何都不发。
        assert_eq!(
            ProviderCompat::standard().effort_for("glm-5.3", Some("high")),
            None
        );
        assert!(!ProviderCompat::standard().supports_effort("glm-5.3"));
    }

    /// 逐档折叠表：`glm-5.3` 代际只接受 `low` / `high` / `max`。
    #[test]
    fn glm_5_3_folds_every_tier() {
        let compat = ProviderCompat::glm();
        for model in ["glm-5.3", "glm-5.3-flash"] {
            assert!(compat.supports_effort(model), "model = {model}");
            for (configured, expected) in [
                ("off", "low"),
                ("minimal", "low"),
                ("low", "low"),
                ("medium", "high"),
                ("high", "high"),
                ("xhigh", "max"),
                ("max", "max"),
            ] {
                assert_eq!(
                    compat.effort_for(model, Some(configured)).as_deref(),
                    Some(expected),
                    "model = {model}, configured = {configured}"
                );
            }
            // 白名单之外的取值不猜映射。
            assert_eq!(compat.effort_for(model, Some("turbo")), None);
            assert_eq!(compat.effort_for(model, None), None);
        }
    }

    /// `glm-5.2` 接受全集但自有映射：`off` / `minimal` 保留为 `minimal`（放弃思考），
    /// `low` / `medium` / `high` 收为 `high`，`xhigh` 收为 `max`。
    #[test]
    fn glm_5_2_folds_every_tier() {
        let compat = ProviderCompat::glm();
        assert!(compat.supports_effort("glm-5.2"));
        for (configured, expected) in [
            ("off", "minimal"),
            ("minimal", "minimal"),
            ("low", "high"),
            ("medium", "high"),
            ("high", "high"),
            ("xhigh", "max"),
            ("max", "max"),
        ] {
            assert_eq!(
                compat.effort_for("glm-5.2", Some(configured)).as_deref(),
                Some(expected),
                "configured = {configured}"
            );
        }
        assert_eq!(compat.effort_for("glm-5.2", Some("turbo")), None);
        assert_eq!(compat.effort_for("glm-5.2", None), None);
    }

    /// GLM 模型级门控：解析得出代际就发，解析不出就 fail-closed。
    /// 实测 5.2 及更早代际也接受该字段，故不按代际阈值门控。
    #[test]
    fn glm_gates_effort_by_model_identity() {
        let compat = ProviderCompat::glm();
        // 解析不出代际 ⇒ 不发。
        for model in ["test", "glm", "glm-", "", "glm-x.3", "glm-5."] {
            assert!(!compat.supports_effort(model), "model = {model}");
            assert_eq!(
                compat.effort_for(model, Some("high")),
                None,
                "model = {model}"
            );
        }
        // 能解析出代际 ⇒ 发（门控不看代际新旧，只看 id 是否可识别）。
        for model in ["glm-5.3", "glm-5.3-flash", "glm-5.2"] {
            assert!(compat.supports_effort(model), "model = {model}");
            assert!(
                compat.effort_for(model, Some("high")).is_some(),
                "model = {model}"
            );
        }
    }

    /// 5.2 之前的代际原样透传合法档位，但 `off` 仍需先映射：
    /// `off` 不在 GLM 合法 wire 值集合内，任何代际都必须映射后才能发。
    ///
    /// 样本用 `glm-5`（省略 minor ⇒ `(5, 0)`）：本仓种子只含 5.3 家族，这条分支
    /// 仅在用户手敲一个更早的 id 时到达，故不拿任何旧发布名当样本。
    #[test]
    fn glm_ids_below_5_2_pass_through_but_never_send_off() {
        let compat = ProviderCompat::glm();
        assert_eq!(compat.effort_for("glm-5", Some("low")), Some("low".into()));
        assert_eq!(
            compat.effort_for("glm-5", Some("xhigh")),
            Some("xhigh".into())
        );
        assert_eq!(
            compat.effort_for("glm-5", Some("medium")),
            Some("medium".into())
        );
        assert_eq!(
            compat.effort_for("glm-5", Some("off")),
            Some("minimal".into())
        );
        // 5.2 分支也有自己的 `off` 规则（保留为 `minimal`）。
        assert_eq!(
            compat.effort_for("glm-5.2", Some("off")),
            Some("minimal".into())
        );
        // 前置：上面那个样本确实落在 `< (5, 2)` 那条分支上。
        assert_eq!(glm_generation("glm-5"), Some((5, 0)));
    }

    /// 回归锚点：无论哪个代际，`off` 都不会原样落到 wire 上。
    #[test]
    fn glm_never_emits_off_on_the_wire() {
        let compat = ProviderCompat::glm();
        for model in ["glm-5.3", "glm-5.3-flash", "glm-5.2"] {
            let sent = compat.effort_for(model, Some("off"));
            assert!(sent.is_some(), "model = {model}");
            assert_ne!(sent.as_deref(), Some("off"), "model = {model}");
        }
    }

    /// 代际解析大小写不敏，小版本后可紧跟任意字符。
    ///
    /// 以下断言只测**解析器对任意 id 的健壮性**，不代表本仓提供那些 id
    /// （种子只有 `glm-5.3` / `glm-5.3-flash`）。
    #[test]
    fn glm_generation_parse_is_case_insensitive() {
        assert_eq!(glm_generation("GLM-5.3"), Some((5, 3)));
        assert_eq!(glm_generation("glm-5.3"), Some((5, 3)));
        assert_eq!(glm_generation("Glm-5.2"), Some((5, 2)));
        assert_eq!(glm_generation("GLM-5.3-Flash"), Some((5, 3)));
        assert_eq!(glm_generation("glm-5.3-flash"), Some((5, 3)));
        // minor 后紧跟非数字字符。
        assert_eq!(glm_generation("glm-5.2v"), Some((5, 2)));
        // 省略 minor ⇒ `(major, 0)`。
        assert_eq!(glm_generation("glm-5"), Some((5, 0)));
        assert_eq!(glm_generation("test"), None);
        assert_eq!(glm_generation("glm"), None);
        assert_eq!(glm_generation("glm-"), None);

        let compat = ProviderCompat::glm();
        assert_eq!(
            compat.effort_for("GLM-5.3", Some("medium")),
            compat.effort_for("glm-5.3", Some("medium"))
        );
        assert_eq!(
            compat.effort_for("GLM-5.3", Some("medium")),
            Some("high".into())
        );
    }

    /// MiMo 的能力位：思考走 `reasoning_content`（同 DeepSeek），流式末包带 usage，
    /// 接受顶层 `reasoning_effort`；tool_calls 仍在 `tool_calls` 里（不是 text）。
    #[test]
    fn mimo_capabilities() {
        let compat = ProviderCompat::mimo();
        assert!(compat.has_reasoning_content);
        assert!(compat.supports_stream_usage);
        assert!(compat.supports_reasoning_effort);
        assert!(!compat.tool_calls_as_text);
    }

    /// MiMo 折叠表**逐档**（实测合法集 = `none|low|medium|high|max`）。
    ///
    /// 实测：`minimal` / `xhigh` / `off` 直接 400（`Param Incorrect`），
    /// `max` 却被接受（错误文案只列了 `none|low|medium|high`，但实测 `max` 返回 200）
    /// —— **以实测为准**，不按文案缩表。
    ///
    /// 变异：把 `off` 映射成 `none` ⇒ 本用例变红（`none` 是「关闭思考」，
    /// 与 `off` 的「最低档」语义不同）；把 `xhigh` 映射成 `high` ⇒ 变红。
    #[test]
    fn mimo_folds_every_tier() {
        let compat = ProviderCompat::mimo();
        for model in ["mimo-v2.6-pro", "mimo-v2.6-flash"] {
            assert!(compat.supports_effort(model), "model = {model}");
            for (configured, expected) in [
                ("off", "low"),
                ("minimal", "low"),
                ("low", "low"),
                ("medium", "medium"),
                ("high", "high"),
                ("xhigh", "max"),
                ("max", "max"),
            ] {
                assert_eq!(
                    compat.effort_for(model, Some(configured)).as_deref(),
                    Some(expected),
                    "model = {model}, configured = {configured}"
                );
            }
            // 白名单之外的取值不猜映射，直接不发。
            assert_eq!(compat.effort_for(model, Some("turbo")), None);
            assert_eq!(compat.effort_for(model, None), None);
        }
    }

    /// MiMo 的模型级门控：**音频模型不发**该字段，识别不出 MiMo 模型也不发。
    ///
    /// 实测音频模型在 `/chat/completions` 上必然 400（`ASR request requires...` /
    /// `messages must contain an assistant role for TTS model`），故它们不属 chat 模型。
    #[test]
    fn mimo_gates_effort_by_model_identity() {
        let compat = ProviderCompat::mimo();
        // 音频模型 ⇒ fail-closed。
        for model in [
            "mimo-v2.5-asr",
            "mimo-v2.5-tts",
            "mimo-v2.5-tts-voiceclone",
            "mimo-v2.5-tts-voicedesign",
        ] {
            assert!(!compat.supports_effort(model), "model = {model}");
            assert_eq!(compat.effort_for(model, Some("high")), None);
        }
        // 用户禁用的模型 ⇒ fail-closed。
        for model in ["mimo-v2.6-pro-ultraspeed", "mimo-v2.5-pro-ultraspeed"] {
            assert!(!compat.supports_effort(model), "model = {model}");
            assert_eq!(compat.effort_for(model, Some("high")), None);
        }
        // 识别不出 MiMo ⇒ fail-closed。
        for model in ["test", "mimo", "mimo-", "", "gpt-4o"] {
            assert!(!compat.supports_effort(model), "model = {model}");
            assert_eq!(compat.effort_for(model, Some("high")), None);
        }
    }

    /// 音频判定按 `-` 分段精确匹配、大小写不敏感。
    ///
    /// `tts-voiceclone` / `tts-voicedesign` 已被 `tts` 覆盖（实测它们就是 TTS 家族）。
    #[test]
    fn mimo_audio_detection_is_segment_exact_and_case_insensitive() {
        assert!(is_mimo_audio_model("mimo-v2.5-asr"));
        assert!(is_mimo_audio_model("mimo-v2.5-tts"));
        assert!(is_mimo_audio_model("mimo-v2.5-tts-voiceclone"));
        assert!(is_mimo_audio_model("mimo-v2.5-tts-voicedesign"));
        assert!(is_mimo_audio_model("MIMO-V2.5-ASR"));
        assert!(!is_mimo_audio_model("mimo-v2.6-pro"));
        assert!(!is_mimo_audio_model("mimo-v2.6-flash"));
        assert!(!is_mimo_audio_model("mimo-v2.6-pro-ultraspeed"));
        // `ultraspeed` 是**用户禁用**而非音频模型 —— 两类分开判。
        assert!(is_mimo_disabled_model("mimo-v2.6-pro-ultraspeed"));
        assert!(!is_mimo_disabled_model("mimo-v2.6-pro"));
        // 禁用的模型也不是本仓可用的 chat 模型。
        assert!(!is_mimo_chat_model("mimo-v2.6-pro-ultraspeed"));
        // 非 `mimo-` 前缀一律不算 MiMo chat 模型。
        assert!(!is_mimo_chat_model("gpt-4o"));
        assert!(is_mimo_chat_model("mimo-v2.6-pro"));
        // 大小写不敏感。
        assert!(is_mimo_chat_model("MIMO-V2.6-PRO"));
    }

    /// **一致性**：目录排除词表必须是「音频词表 ∪ 禁用词表」。
    ///
    /// 两处各写一遍必然漂移：目录里放行了一个模型、模型级门控却不认它 ⇒
    /// 用户选中就 400（且 `/login` 会把它当起始模型）。
    ///
    /// 变异：给目录词表加一个词而不加到音频 / 禁用两份里 ⇒ 本用例变红。
    #[test]
    fn mimo_excluded_tokens_cover_audio_and_disabled() {
        for token in MIMO_AUDIO_TOKENS.iter().chain(MIMO_DISABLED_TOKENS.iter()) {
            assert!(MIMO_EXCLUDED_TOKENS.contains(token), "目录词表漏了 {token}");
        }
        assert_eq!(
            MIMO_EXCLUDED_TOKENS.len(),
            MIMO_AUDIO_TOKENS.len() + MIMO_DISABLED_TOKENS.len(),
            "目录词表不应有音频 / 禁用两份之外的词"
        );
    }

    /// 回归锚点：MiMo 的 wire 值**永远不出现**实测非法档位。
    ///
    /// 变异：把折叠表改回逐字透传 ⇒ 本用例变红。
    #[test]
    fn mimo_never_emits_illegal_tiers_on_the_wire() {
        let compat = ProviderCompat::mimo();
        for model in ["mimo-v2.6-pro", "mimo-v2.6-flash"] {
            for configured in ["off", "minimal", "xhigh"] {
                let sent = compat.effort_for(model, Some(configured));
                assert!(sent.is_some(), "model = {model}, configured = {configured}");
                let sent = sent.unwrap();
                assert!(
                    ["none", "low", "medium", "high", "max"].contains(&sent.as_str()),
                    "model = {model}, configured = {configured} ⇒ 非法 wire 值 {sent}"
                );
            }
        }
    }
}
