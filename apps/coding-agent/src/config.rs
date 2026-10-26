use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;
use ys_core::EffortLevel;
use ys_model::Model;

use crate::provider::ProviderRegistry;

/// 默认推理强度。`high` 是该模型可接受的最低默认档，`low` 只留给显式实验。
pub(crate) const DEFAULT_REASONING_EFFORT: &str = "high";

/// Model factory 函数类型。封装适配器特有的构建逻辑。
/// config 不完整（缺 api_base/api_key）时返回 None。
type ModelFactory = Box<dyn Fn(&Config) -> Option<Box<dyn Model>> + Send + Sync>;

/// 按**模型名**构造的 factory（subagent 的 `model` 参数）。
type ModelFactoryFor = Box<dyn Fn(&Config, &str) -> Option<Box<dyn Model>> + Send + Sync>;

/// `~/.yushan/config.json` 的**原始**反序列化形态（**用户手写**，故宽容解析）。
///
/// 值先落 `serde_json::Value`，再由 [`parse_user_config`] 逐项校验 —— 这样
/// **一条非法条目只丢自己**，不会把整份文件打回默认。未知字段由 serde 默认忽略
/// （向后兼容：将来加字段不影响旧文件，旧 app 读新文件也不炸）。
///
/// `#[serde(default)]` 的范式照 `ys_protocol::Request::Login.api_base`：全部字段可选。
#[derive(Debug, Default, Deserialize)]
struct RawUserConfig {
    /// model → context window（token）覆盖。**键是 model，不是 provider**。
    #[serde(default)]
    context_window: HashMap<String, serde_json::Value>,
    #[serde(default)]
    compaction: RawCompaction,
    /// 单回合模型调用轮数上限。**顶层标量**（回合属性，不按 model 分）。
    #[serde(default)]
    max_rounds: Option<serde_json::Value>,
    /// 一次用户请求（含复核回合）的 wall-clock 预算，单位**秒**。
    #[serde(default)]
    max_turn_duration_secs: Option<serde_json::Value>,
    /// 收场前等待在跑子 agent 的上限，单位**秒**（ADR-0055）。
    #[serde(default)]
    subagent_wait_secs: Option<serde_json::Value>,
    #[serde(default)]
    model_retry: RawModelRetry,
    #[serde(default)]
    model_timeout: RawModelTimeout,
}

/// `config.json` 的 `compaction` 段。字段全可选，未给的用 `CompactPolicy::default()`。
#[derive(Debug, Default, Deserialize)]
struct RawCompaction {
    #[serde(default)]
    trigger_ratio: Option<serde_json::Value>,
    #[serde(default)]
    keep_ratio: Option<serde_json::Value>,
}

/// `config.json` 的 `model_retry` 段。字段全可选，未给的用 [`ModelRetryConfig::default`]。
#[derive(Debug, Default, Deserialize)]
struct RawModelRetry {
    #[serde(default)]
    max_attempts: Option<serde_json::Value>,
    #[serde(default)]
    base_delay_ms: Option<serde_json::Value>,
    #[serde(default)]
    max_delay_ms: Option<serde_json::Value>,
}

/// `config.json` 的 `model_timeout` 段。字段全可选，未给的用 [`ModelTimeoutConfig::default`]。
#[derive(Debug, Default, Deserialize)]
struct RawModelTimeout {
    #[serde(default)]
    connect_secs: Option<serde_json::Value>,
    #[serde(default)]
    idle_secs: Option<serde_json::Value>,
}

/// 模型调用的有界重试策略（ADR-0028 决策 4）。
///
/// 是**回合 / 进程属性**，与 model 无关 —— 故落顶层对象段，**不做「按 model 的 map」**
/// （照 ADR-0027 决策 1 对 `max_rounds` 的同一条理由：重试/超时不属于某个 model 的方言）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelRetryConfig {
    /// **额外**重试次数（不含首次）。`0` = 不重试。
    pub max_attempts: u32,
    /// 指数退避基数（毫秒）。
    pub base_delay_ms: u64,
    /// 单次退避硬上限（毫秒），兼作 `Retry-After` 的硬上限。
    pub max_delay_ms: u64,
}

impl Default for ModelRetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay_ms: 500,
            max_delay_ms: 30_000,
        }
    }
}

/// 模型调用的两个超时旋钮（ADR-0028 决策 5）：连接建立 + 流空闲（多久无字节算超时）。
///
/// 同样是**回合 / 进程属性**，与 model 无关 —— 故落顶层对象段，**不做「按 model 的 map」**
/// （同 [`ModelRetryConfig`]，照 ADR-0027 决策 1 对 `max_rounds` 的同一条理由）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelTimeoutConfig {
    /// 连接建立超时（秒）。
    pub connect_secs: u64,
    /// 流空闲超时（秒）—— 多久无字节即判定超时。
    pub idle_secs: u64,
}

impl Default for ModelTimeoutConfig {
    fn default() -> Self {
        Self {
            connect_secs: 15,
            idle_secs: 120,
        }
    }
}

/// **已校验**的用户配置覆盖项（`config.json` 的规整形态）。
///
/// 与 [`RawUserConfig`] 分开：读一次、校验一次，解析路径（[`Config::context_window`]）
/// 只管查表，不再重复做类型判断。
#[derive(Debug, Clone, Default)]
struct UserConfig {
    /// model → context window（仅正整数）
    windows: HashMap<String, usize>,
    /// 触发比例覆盖（仅有限值且落 `(0, 1]`）
    trigger_ratio: Option<f64>,
    /// 保留比例覆盖（同上）
    keep_ratio: Option<f64>,
    /// 单回合模型调用轮数上限覆盖（仅正整数）。`None` = 未给 ⇒ 回落 `u32::MAX`。
    max_rounds: Option<u32>,
    /// wall-clock 预算秒数覆盖（仅正整数）。`None` = 未给 ⇒ 回落 48h。
    max_turn_duration_secs: Option<u64>,
    /// 收场前等待在跑子 agent 的上限秒数（仅正整数）。`None` = 未给 ⇒ 回落 30min。
    ///
    /// 与 `max_turn_duration_secs` **不是同一个东西**：后者是「一个请求能占多少墙钟」，
    /// 本值是「模型已经答完、子 agent 还在跑时，父回合愿意为它再挂多久」。
    subagent_wait_secs: Option<u64>,
    /// 重试策略（逐字段独立，缺省已由 [`ModelRetryConfig::default`] 填好）。
    model_retry: ModelRetryConfig,
    /// 超时旋钮（逐字段独立，缺省已由 [`ModelTimeoutConfig::default`] 填好）。
    model_timeout: ModelTimeoutConfig,
}

/// context window 的**来源**（四级，与 [`Config::context_window_with_source_for`] 一一对应）。
///
/// 单说「窗口多大」不说明**可信度**：1000000 可能来自厂商目录（保守但真实），也可能来自
/// 兜底默认值（压根没人报过）。用户看到来源才知道**该不该自己写一份覆盖**。
///
/// 来源与窗口值**成对**产出（同一个判定函数）—— 拆开两次判定会漂移，值与来源会对不上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextWindowSource {
    /// ① 用户级 `config.json` 的 `context_window` 覆盖（最具体，用户说了算）。
    UserOverride,
    /// ② **provider 目录**（`/v1/models`）里厂商自己报的窗口。
    ProviderCatalog,
    /// ③ 内建兜底表 [`crate::provider::builtin_context_window`]（目录报不出时的第二手来源）。
    BuiltinTable,
    /// ④ 兜底默认值（`RunLimits::default().context_window` = 1_000_000）。
    Default,
}

impl ContextWindowSource {
    /// 面向用户的短标签（`/model` 输出里的 `source: <标签>`）。
    pub fn label(self) -> &'static str {
        match self {
            Self::UserOverride => "user override",
            Self::ProviderCatalog => "provider catalog",
            Self::BuiltinTable => "builtin table",
            Self::Default => "default",
        }
    }
}

impl std::fmt::Display for ContextWindowSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

pub struct Config {
    pub api_base: Option<String>,
    pub api_key: Option<String>,
    pub model: String,
    pub cwd: PathBuf,
    pub provider: Option<String>,
    /// 会话级思考档位覆盖（`/effort`）。`None` = 无覆盖 ⇒ 使用
    /// [`Config::reasoning_effort`] 的配置默认（`YUSHAN_REASONING_EFFORT`，默认
    /// `high`）。随会话 meta 恢复与持久化，但**不**进 `config.json`。
    pub effort: Option<EffortLevel>,
    pub registry: ProviderRegistry,
    /// 启动期扫描到的 skill（**已去重、已按 `command` 升序**）。
    ///
    /// 由 [`Config::load_skills`] 写入。三处消费者：系统提示的目录段（`main` 经
    /// [`Config::load_skills`] 的返回值之外的 `skills::catalogue`）、`/` 浮层的候选
    /// （`view::build_view` 只取 `command`/`label`/`description`，**路径不外泄**）、
    /// `Request::Prompt` 的 `/skill-name` 展开（`app_loop`）。
    ///
    /// ⚠️ **绝不写进 `state.json`** —— 那是 `StateStore::save()` 会**覆盖**的文件。
    pub skills: Vec<crate::skills::SkillEntry>,
    /// 因整表字节预算被收起的 skill 条数（`Catalogue.omitted`，R3 的可见性出口）。
    pub skills_omitted: usize,
    /// 可选推理强度（`YUSHAN_REASONING_EFFORT`）。`None` = 不发送 provider 字段。
    reasoning_effort: Option<String>,
    /// 从 `~/.yushan/config.json` 读到的覆盖项（用户写、app 只读）。
    ///
    /// 由 [`Config::load_user_config`] 写入，经 [`Config::context_window`] /
    /// [`Config::compact_policy`] 读出（`main.rs` 装配进 `RunLimits`）。
    user_config: UserConfig,
    model_factory: Option<ModelFactory>,
    model_factory_for: Option<ModelFactoryFor>,
}

impl Config {
    /// 从环境变量加载。除 `cwd` 外所有字段都是可选的。
    /// agent 可在无 API 凭证时启动；通过 `/login` 配置。
    pub fn from_env() -> Result<Self, String> {
        let api_base = std::env::var("YUSHAN_API_BASE")
            .or_else(|_| std::env::var("OPENAI_API_BASE"))
            .ok();
        let api_key = std::env::var("YUSHAN_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .ok();
        // **不写死模型名**：没给就留空，由 `main` 从该端点的 `/v1/models` 目录取
        // （`YUSHAN_API_BASE` + `YUSHAN_API_KEY` 直连时）。
        let model = std::env::var("YUSHAN_MODEL").unwrap_or_default();
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let reasoning_effort =
            parse_reasoning_effort(std::env::var("YUSHAN_REASONING_EFFORT").ok().as_deref())
                .or_else(|| Some(DEFAULT_REASONING_EFFORT.to_string()));

        Ok(Self {
            api_base,
            api_key,
            model,
            cwd,
            provider: None,
            effort: None,
            registry: ProviderRegistry::new(),
            skills: Vec::new(),
            skills_omitted: 0,
            reasoning_effort,
            user_config: UserConfig::default(),
            model_factory: None,
            model_factory_for: None,
        })
    }

    pub fn is_configured(&self) -> bool {
        self.api_base.is_some() && self.api_key.is_some()
    }

    /// 解析当前 model 的 context window（四级）：
    ///
    /// ① `~/.yushan/config.json` 里该 model 的覆盖（最具体，用户说了算）
    /// ② **provider 目录里的 `context_window`**（厂商在 `/v1/models` 里自己报的）
    /// ③ 内置兜底表 [`crate::provider::builtin_context_window`]（仅目录报不出窗口的模型）
    /// ④ `RunLimits::default().context_window` = 1_000_000（兜底）
    ///
    /// **每回合都要重算**：`/login`、`/model` 会在运行中换 model，只在启动时算一次
    /// 会让「登到 1M 模型后仍按 128k 压缩」的 bug 复活。
    ///
    /// 生产调用点：`main.rs::resolve_limits`（启动初值）与 `app_loop`（每回合刷新）。
    pub fn context_window(&self) -> usize {
        self.context_window_for(&self.model)
    }

    /// 按**给定模型名**解析 context window（同 [`Self::context_window`] 的四级）：
    /// ① `config.json` 覆盖 > ② 目录 > ③ 内置兜底表 > ④ `RunLimits::default().context_window`。
    ///
    /// 供 subagent 按 `model` 参数取子模型窗口用 —— 子 agent 换模型却继承父窗口是
    /// **静默错**（32k 模型按 1M 判 ⇒ 不压缩 ⇒ 撞窗）。
    pub fn context_window_for(&self, model: &str) -> usize {
        self.context_window_with_source_for(model).0
    }

    /// 按**给定模型名**解析 context window **及其来源**（四级，与
    /// [`Self::context_window_for`] **同一个判定**）。
    ///
    /// 窗口值与来源**成对**产出：分开两次判定会漂移 —— 值取自一处、来源取自另一处，
    /// `/model` 就会显示「窗口 258400（来源：默认）」这种自相矛盾的话。
    pub fn context_window_with_source_for(&self, model: &str) -> (usize, ContextWindowSource) {
        if let Some(&window) = self.user_config.windows.get(model) {
            return (window, ContextWindowSource::UserOverride);
        }
        if let Some(window) = self.registry.catalog_context_window(model) {
            return (window, ContextWindowSource::ProviderCatalog);
        }
        if let Some(window) = crate::provider::builtin_context_window(model) {
            return (window, ContextWindowSource::BuiltinTable);
        }
        (
            ys_component::RunLimits::default().context_window,
            ContextWindowSource::Default,
        )
    }

    /// 当前 model 的 context window **及其来源**（同 [`Self::context_window_with_source_for`]）。
    pub fn context_window_with_source(&self) -> (usize, ContextWindowSource) {
        self.context_window_with_source_for(&self.model)
    }

    /// 压缩策略：`config.json` 的比例覆盖叠在 [`ys_component::CompactPolicy::default`] 上。
    /// 只给 `trigger_ratio` 时 `keep_ratio` 仍是默认值（逐字段独立）。
    pub fn compact_policy(&self) -> ys_component::CompactPolicy {
        let mut policy = ys_component::CompactPolicy::default();
        if let Some(ratio) = self.user_config.trigger_ratio {
            policy.trigger_ratio = ratio;
        }
        if let Some(ratio) = self.user_config.keep_ratio {
            policy.keep_ratio = ratio;
        }
        policy
    }

    /// 单回合模型调用轮数上限（`config.json` 的 `max_rounds`）。
    ///
    /// 缺省回落 [`u32::MAX`]（= 不设）—— 与库的保守默认
    /// [`ys_component::RunLimits::default().max_rounds`]（`10`）**刻意分离**：
    /// 库给保守默认，app 的产品默认是不设（ADR-0027 决策 1）。
    pub fn max_rounds(&self) -> u32 {
        self.user_config.max_rounds.unwrap_or(u32::MAX)
    }

    /// 一次用户请求（含复核回合）的 wall-clock 预算（`config.json` 的
    /// `max_turn_duration_secs`）。缺省 48h —— 需求是 48 小时长程任务，
    /// 真正的兜底是这条**小时级**阀门，不是轮数（ADR-0027 决策 6）。
    pub fn max_turn_duration(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.user_config.max_turn_duration_secs.unwrap_or(172_800))
    }

    /// 收场前等待在跑子 agent 的上限（`config.json` 的 `subagent_wait_secs`，ADR-0055）。
    ///
    /// 缺省 **30 min**：与 [`max_turn_duration`](Self::max_turn_duration) **刻意分离** ——
    /// 后者是「一个请求能占多少墙钟」（48h，长程任务需要），本值是「模型已经答完、
    /// 但子 agent 还在跑时，父回合愿意为它再挂多久」。实测最长一次派发 965 s，
    /// 30 min 留约 2 倍余量。
    ///
    /// 调用方仍须再取 `min(本值, 回合剩余预算)` —— 见 `TurnCtx::deadline_remaining`。
    pub fn subagent_wait(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.user_config.subagent_wait_secs.unwrap_or(30 * 60))
    }

    /// 配置层推理强度（provider 的 `reasoning_effort`）。缺省 `high`。
    ///
    /// 这是 `/effort` 覆盖之前的值；请求实际发送的是
    /// [`Config::effective_reasoning_effort`]。
    pub fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning_effort.as_deref()
    }

    /// 请求实际使用的推理强度：会话覆盖优先，否则回落配置默认。
    pub fn effective_reasoning_effort(&self) -> Option<String> {
        self.effort
            .map(|effort| effort.as_wire().to_string())
            .or_else(|| self.reasoning_effort().map(str::to_owned))
    }

    /// 请求实际生效的思考档位（用于 `/effort` 无参与 `/status` 显示）。
    ///
    /// 配置默认无法解析成 [`EffortLevel`] 时返回 `None`（`parse_reasoning_effort`
    /// 已做白名单，生产上不会发生）。
    pub fn effective_effort_level(&self) -> Option<EffortLevel> {
        self.effort.or_else(|| {
            self.reasoning_effort()
                .and_then(|value| value.parse::<EffortLevel>().ok())
        })
    }

    /// 模型调用重试策略（`config.json` 的 `model_retry`，ADR-0028 决策 4）。
    pub fn model_retry(&self) -> ModelRetryConfig {
        self.user_config.model_retry
    }

    /// 模型调用超时（`config.json` 的 `model_timeout`，ADR-0028 决策 5）。
    pub fn model_timeout(&self) -> ModelTimeoutConfig {
        self.user_config.model_timeout
    }

    /// 读 `~/.yushan/config.json`（**用户写、app 只读**）。
    ///
    /// 容错三档，**绝不阻断启动**、**绝不 panic**：
    /// - 文件不存在 → 什么都不做（全默认），**不落日志**（照
    ///   [`crate::provider::ProviderRegistry::load_auth`] 的惯例：用户没写配置文件
    ///   是常态，不是故障）
    /// - 整份畸形 → 落日志后**整份忽略**，沿用调用前的状态
    /// - 单条值非法 → 只丢该条（落日志），同文件其余条目照常生效
    ///
    /// 接**显式 `path`** 而非内部拼 `$HOME`：测试可直接传临时文件，**不必改写 `HOME`**
    /// —— 与 `test_env::env_lock` 的竞争面就此消失。
    ///
    /// ⚠️ **不要**把这份配置写进 `~/.yushan/state.json` —— 那是
    /// `StateStore::save()` 会**覆盖**的文件（`state.rs`）。
    pub fn load_user_config(&mut self, path: &std::path::Path) {
        let data = match std::fs::read_to_string(path) {
            Ok(data) => data,
            Err(_) => return,
        };
        match parse_user_config(&data) {
            Ok(user) => self.user_config = user,
            // 库内部诊断 → 日志文件（设计 §5）：启动期 TUI 可能已起屏，
            // stderr 会冲掉整屏，而这里没有「历史输出」可看。
            Err(e) => crate::logging::log(&format!("failed to parse config.json: {e}")),
        }
    }

    /// 默认用户配置路径：`~/.yushan/config.json`（无 HOME 时为 `None`）。
    pub fn default_user_config_path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
        Some(PathBuf::from(home).join(".yushan").join("config.json"))
    }

    /// skill 扫描根：`[<cwd>/.yushan/skills, <home>/.yushan/skills]`。
    ///
    /// # 顺序即语义（**不是**风格问题）
    ///
    /// [`crate::skills::scan`] 靠**位置**判定 `scope`：`roots[0]` ⇒ `Scope::Project`、
    /// `roots[1..]` ⇒ `Scope::User`，而同名（命令名 = 目录名）时 **project 胜**（整条
    /// 替换）。故**项目级必须在前** —— 顺序错了就是静默的语义错误（项目本地覆盖失效，
    /// 用户级反而遮蔽项目级）。本函数是该顺序在生产侧的**唯一来源**。
    ///
    /// 无 `HOME` / `USERPROFILE` 时只返回项目那一条（照
    /// [`Config::default_user_config_path`] 的读法）。**不扫 `.claude/skills`**
    /// （设计 D2；JackCC 靠软链进 `~/.yushan/skills`）。
    ///
    /// # 去重（`cwd == $HOME` 时）
    ///
    /// 从 home 目录启动时两条根算出来是**同一路径**。若照发两条，[`crate::skills::scan`]
    /// 会把每个 skill 收两遍（一条 `Project`、一条 `User`，`path` 相同），于是
    /// `shadow_diagnostics` 对**每个** skill 产一条**假的**「project 级遮蔽 user 级」
    /// 诊断 —— 几十行纯噪音恰好淹没 R4（「一个仓库能静默换掉你的 `/git-status`」）依赖
    /// 的那个信号，启动期 IO 也翻倍。故两条路径相等时只返回一条。
    ///
    /// ⚠️ 注意：**不得**为此改 [`crate::skills::scan`] 的 scope-by-position 契约 ——
    /// 去重只发生在「同一路径」这一退化情形，三条以上的根顺序语义原样保留。
    pub fn default_skill_roots(cwd: &std::path::Path) -> Vec<PathBuf> {
        let project = cwd.join(".yushan").join("skills");
        let mut roots = vec![project.clone()];
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            let user = PathBuf::from(home).join(".yushan").join("skills");
            if user != project {
                roots.push(user);
            }
        }
        roots
    }

    /// 扫描 skill 根：条目落 `self.skills`、预算收束计数落 `self.skills_omitted`，
    /// **诊断作为返回值**交给调用方打印（本函数**绝不 print** —— 与
    /// `capabilities.rs` / `toolset.rs` 同规）。
    ///
    /// 入参走[`Config::default_skill_roots`]给出的显式路径（`scan` **不读 env**）。
    /// 空目录 / 目录不存在 ⇒ 零条目、零诊断（静默，不是故障）。
    ///
    /// **不写盘**：`skills` 是纯内存快照，绝不进 `~/.yushan/state.json`
    /// （那是 app 会 `save()` 覆盖的文件）。
    pub fn load_skills(&mut self, roots: &[PathBuf]) -> Vec<String> {
        let (entries, diags) = crate::skills::scan(roots);
        let catalogue = crate::skills::catalogue(entries, crate::skills::Budget::default());
        self.skills_omitted = catalogue.omitted;
        self.skills = catalogue.entries;
        diags
    }

    /// 设置 model factory。适配器类型确定后在 main.rs 中调用一次。
    pub fn set_model_factory(
        &mut self,
        factory: impl Fn(&Config) -> Option<Box<dyn Model>> + Send + Sync + 'static,
    ) {
        self.model_factory = Some(Box::new(factory));
    }

    /// 从当前 config 构建 model。委托给 factory。
    pub fn build_model(&self) -> Option<Box<dyn Model>> {
        self.model_factory.as_ref().and_then(|f| f(self))
    }

    /// 注册「按**模型名**构造」的 factory（subagent 的 `model` 参数用）。
    ///
    /// 与 [`Config::set_model_factory`] 分开：后者的语义是「按 `config.model`」，
    /// 这条是「按调用方给的名字」。未注册时 [`Config::build_model_for`] 恒 `None`
    /// ⇒ 调用方 fail-closed（**绝不静默回退**到当前模型）。
    pub fn set_model_factory_for(
        &mut self,
        factory: impl Fn(&Config, &str) -> Option<Box<dyn Model>> + Send + Sync + 'static,
    ) {
        self.model_factory_for = Some(Box::new(factory));
    }

    /// 按**给定模型名**构造 model。未注册 factory 或无法构造 ⇒ `None`。
    pub fn build_model_for(&self, name: &str) -> Option<Box<dyn Model>> {
        self.model_factory_for.as_ref().and_then(|f| f(self, name))
    }

    /// 返回当前 provider 的 ProviderCompat（未设置时为 "custom"）。
    pub fn current_compat(&self) -> ys_model_openai_compat::compat::ProviderCompat {
        let name = self.provider.as_deref().unwrap_or("custom");
        self.registry.compat_for(name)
    }

    /// 当前 provider **与当前模型** 是否接受 `reasoning_effort`。
    ///
    /// 与 `ProviderCompat::supports_reasoning_effort` 的区别：后者只答「这个 provider 认不认
    /// 该字段」，本函数还要求该模型 id 能被识别（GLM 需解析出 `glm-<major>.<minor>`，解析不出
    /// 一律 fail-closed）。面向用户的「本次请求会不会发送思考字段」一律用本函数。
    pub fn accepts_reasoning_effort(&self) -> bool {
        self.current_compat().supports_effort(&self.model)
    }

    /// 当前 provider / api_base 要求的额外请求头（透传给适配器）。
    ///
    /// 生产消费者：`main.rs` 的两个 model factory —— 每次 `build_model` 都重取，
    /// 故 `/login`、`/model` 换 provider 后头也跟着换。
    pub fn request_headers(&self) -> Vec<(String, String)> {
        self.registry
            .request_headers(self.provider.as_deref(), self.api_base.as_deref())
    }
}

/// 解析推理强度。只接受主流 OpenAI 兼容取值；非法值落诊断后由调用方回落默认 `high`。
fn parse_reasoning_effort(value: Option<&str>) -> Option<String> {
    let value = value?.trim().to_ascii_lowercase();
    if value.is_empty() {
        return None;
    }
    if matches!(
        value.as_str(),
        "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
    ) {
        Some(value)
    } else {
        crate::logging::log(&format!(
            "ignoring YUSHAN_REASONING_EFFORT: expected minimal|low|medium|high|xhigh|max, got {value}"
        ));
        None
    }
}

/// 解析 + 校验 `config.json` 文本。`Err` 仅表示**整份** JSON 畸形；
/// 单条非法由本函数**就地丢弃 + 落日志**，不进 `Err`。
fn parse_user_config(data: &str) -> Result<UserConfig, serde_json::Error> {
    let raw: RawUserConfig = serde_json::from_str(data)?;
    let mut user = UserConfig::default();

    for (model, value) in raw.context_window {
        // `as_u64` 同时挡下负值、浮点与字符串；0 另有语义（窗口为 0 会让压缩退化），
        // 一并按非法处理。
        match value.as_u64().and_then(|n| usize::try_from(n).ok()) {
            Some(window) if window > 0 => {
                user.windows.insert(model, window);
            }
            _ => crate::logging::log(&format!(
                "ignoring config.json context_window[\"{model}\"]: \
                 expected a positive integer, got {value}"
            )),
        }
    }

    user.trigger_ratio = sanitize_ratio("trigger_ratio", raw.compaction.trigger_ratio);
    user.keep_ratio = sanitize_ratio("keep_ratio", raw.compaction.keep_ratio);

    // 标量：正整数（0 会让轮数上限立刻到顶 / 预算立刻到点，一并按非法处理）。
    user.max_rounds = sanitize_uint("max_rounds", raw.max_rounds, 1u32);
    user.max_turn_duration_secs =
        sanitize_uint("max_turn_duration_secs", raw.max_turn_duration_secs, 1u64);
    user.subagent_wait_secs = sanitize_uint("subagent_wait_secs", raw.subagent_wait_secs, 1u64);

    // 重试 / 超时**逐字段独立**覆盖：未给的项保持默认（照 `compaction` 的
    // `trigger_ratio` / `keep_ratio` 关系）。重试三项允许 0（`max_attempts: 0` = 不重试、
    // 延迟 0 = 不退避），超时两项不允许 0（0 无意义）。
    let mut retry = ModelRetryConfig::default();
    if let Some(n) = sanitize_uint(
        "model_retry.max_attempts",
        raw.model_retry.max_attempts,
        0u32,
    ) {
        retry.max_attempts = n;
    }
    if let Some(n) = sanitize_uint(
        "model_retry.base_delay_ms",
        raw.model_retry.base_delay_ms,
        0u64,
    ) {
        retry.base_delay_ms = n;
    }
    if let Some(n) = sanitize_uint(
        "model_retry.max_delay_ms",
        raw.model_retry.max_delay_ms,
        0u64,
    ) {
        retry.max_delay_ms = n;
    }
    user.model_retry = retry;

    let mut timeout = ModelTimeoutConfig::default();
    if let Some(n) = sanitize_uint(
        "model_timeout.connect_secs",
        raw.model_timeout.connect_secs,
        1u64,
    ) {
        timeout.connect_secs = n;
    }
    if let Some(n) = sanitize_uint("model_timeout.idle_secs", raw.model_timeout.idle_secs, 1u64) {
        timeout.idle_secs = n;
    }
    user.model_timeout = timeout;

    Ok(user)
}

/// 「取无符号整数 + 落日志」的通用校验（`max_rounds` / `max_turn_duration_secs` /
/// `model_retry` / `model_timeout` 共用）。`min` 是最小合法值（标量与超时旋钮取 1、
/// 重试三项取 0）。
///
/// `as_u64()` 同时挡下负值、浮点、字符串、布尔、对象、数组；再与 `min` 比较。
/// `None` 入参（未给）**静默**返回 `None`；给了值但非法则落一行日志并返回 `None`，
/// 于是**只丢该条**，同文件其余条目照常生效（照 [`sanitize_ratio`] 的宽容语义）。
fn sanitize_uint<T>(path: &str, value: Option<serde_json::Value>, min: T) -> Option<T>
where
    T: TryFrom<u64> + PartialOrd + std::fmt::Display,
{
    let value = value?;
    match value.as_u64().and_then(|n| T::try_from(n).ok()) {
        Some(n) if n >= min => Some(n),
        _ => {
            crate::logging::log(&format!(
                "ignoring config.json {path}: \
                 expected an integer >= {min}, got {value}"
            ));
            None
        }
    }
}

/// 比例校验：必须是**有限**值且落 `(0, 1]`。`None` 表示「未给 / 非法」→ 用默认值。
///
/// 上界取 1.0 而非 `< 1.0`：`trigger_ratio = 1.0`（用满窗口才压）是合法意图，
/// 且 `CompactPolicy::trigger_at` 自己会用 `output_reserve` 钳住，不会溢出。
fn sanitize_ratio(key: &str, value: Option<serde_json::Value>) -> Option<f64> {
    let value = value?;
    match value.as_f64() {
        Some(ratio) if ratio.is_finite() && ratio > 0.0 && ratio <= 1.0 => Some(ratio),
        _ => {
            crate::logging::log(&format!(
                "ignoring config.json compaction.{key}: \
                 expected a ratio in (0, 1], got {value}"
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ApiModel;
    use crate::test_env::{EnvRestore, env_lock};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 直接构造 `Config`（不经 `from_env`）：本模块的配置解析测试**不读环境变量**，
    /// 因而不必持 `env_lock` —— 与改写 env 的测试零竞争。
    fn test_config(model: &str) -> Config {
        Config {
            api_base: None,
            api_key: None,
            model: model.to_string(),
            cwd: PathBuf::from("/tmp"),
            provider: None,
            effort: None,
            registry: ProviderRegistry::new(),
            skills: Vec::new(),
            skills_omitted: 0,
            reasoning_effort: None,
            user_config: UserConfig::default(),
            model_factory: None,
            model_factory_for: None,
        }
    }

    /// 临时目录名含**进程内原子序号 + `process::id()`** —— 并行测试不得撞名
    /// （`SystemTime::as_nanos()` 在 macOS 只到微秒级，会撞）。
    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("yushan_config_{name}_{}_{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 把一段 JSON 写进临时 config.json，返回 (目录, 文件路径)。调用方负责删目录。
    fn write_config(name: &str, content: &str) -> (PathBuf, PathBuf) {
        let dir = temp_dir(name);
        let path = dir.join("config.json");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    /// `Config::from_env` 读取的 API 凭证相关环境变量。
    const API_ENV_KEYS: &[&str] = &[
        "YUSHAN_API_BASE",
        "YUSHAN_API_KEY",
        "YUSHAN_REASONING_EFFORT",
        "OPENAI_API_BASE",
        "OPENAI_API_KEY",
    ];

    /// 清空 [`API_ENV_KEYS`]，返回 Drop 时自动恢复原值的 guard。
    /// 调用方必须先持有 [`env_lock`]。
    fn clear_api_env() -> EnvRestore {
        let env = EnvRestore::capture(API_ENV_KEYS);
        // SAFETY: 调用方持有 env_lock，独占这些变量的修改与恢复。
        unsafe {
            for key in API_ENV_KEYS {
                std::env::remove_var(key);
            }
        }
        env
    }

    // 测试用的 dummy model
    struct DummyModel;

    #[async_trait::async_trait]
    impl ys_model::Model for DummyModel {
        fn model_id(&self) -> &str {
            "dummy"
        }
        async fn complete(
            &self,
            _request: ys_model::ModelRequest,
            _sink: &mut dyn ys_model::ModelEventSink,
        ) -> Result<ys_model::ModelResponse, ys_model::ModelError> {
            unimplemented!("test dummy")
        }
    }

    /// `build_model_for`：未注册 factory ⇒ `None`（fail-closed 的基础）；
    /// 注册后按名字构造，名字不认识仍返回 `None`。
    #[test]
    fn test_config_model_factory_for() {
        let _guard = env_lock();
        let _env = clear_api_env();
        let mut config = Config::from_env().unwrap();
        assert!(
            config.build_model_for("anything").is_none(),
            "未注册 factory 时必须返回 None（fail-closed）"
        );
        config.set_model_factory_for(|_, name| {
            (name == "ok").then(|| Box::new(DummyModel) as Box<dyn ys_model::Model>)
        });
        assert!(config.build_model_for("ok").is_some());
        assert!(config.build_model_for("nope").is_none());
    }

    #[test]
    fn test_config_from_env_no_vars() {
        let _guard = env_lock();
        let _env = clear_api_env();

        let config = Config::from_env().unwrap();
        assert!(!config.is_configured());
        assert!(config.build_model().is_none());
        assert!(config.registry.find_provider("deepseek").is_some());
        assert_eq!(config.reasoning_effort(), Some(DEFAULT_REASONING_EFFORT));
    }

    /// 推理强度只接受已知取值；大小写与空白归一化，非法值回落默认 `high`。
    ///
    /// 变异：去掉 `to_ascii_lowercase()` ⇒ 第一条变红；去掉白名单 ⇒ 非法值断言变红。
    #[test]
    fn reasoning_effort_is_optional_and_validated() {
        let _guard = env_lock();
        let _env = clear_api_env();

        // SAFETY: 持有 env_lock。
        unsafe { std::env::set_var("YUSHAN_REASONING_EFFORT", " LoW ") };
        assert_eq!(Config::from_env().unwrap().reasoning_effort(), Some("low"));

        // SAFETY: 持有 env_lock。
        unsafe { std::env::set_var("YUSHAN_REASONING_EFFORT", "please-think-harder") };
        assert_eq!(
            Config::from_env().unwrap().reasoning_effort(),
            Some(DEFAULT_REASONING_EFFORT)
        );
    }

    /// 有效强度：会话覆盖优先，清除覆盖后回落到配置默认（env 或 `high`）。
    ///
    /// 变异：把 `effective_reasoning_effort` 的优先级反转 ⇒ 第一条覆盖断言变红；
    /// 去掉回落分支 ⇒ `None` 覆盖后的断言变红。
    #[test]
    fn effective_effort_prefers_override_then_default() {
        let _guard = env_lock();
        let _env = clear_api_env();
        let mut config = Config::from_env().unwrap();

        assert_eq!(config.effective_reasoning_effort().as_deref(), Some("high"));
        assert_eq!(config.effective_effort_level(), Some(EffortLevel::High));

        config.effort = Some(EffortLevel::Max);
        assert_eq!(config.effective_reasoning_effort().as_deref(), Some("max"));
        assert_eq!(config.effective_effort_level(), Some(EffortLevel::Max));

        config.effort = None;
        assert_eq!(config.effective_reasoning_effort().as_deref(), Some("high"));

        // SAFETY: 持有 env_lock。
        unsafe { std::env::set_var("YUSHAN_REASONING_EFFORT", "low") };
        let mut config = Config::from_env().unwrap();
        assert_eq!(config.effective_reasoning_effort().as_deref(), Some("low"));
        config.effort = Some(EffortLevel::XHigh);
        assert_eq!(
            config.effective_reasoning_effort().as_deref(),
            Some("xhigh"),
            "覆盖优先于 env 默认"
        );
    }

    /// `context_window_for(name)` 与 `context_window()` 对当前 model 必须一致；
    /// 未知模型回落到 `RunLimits::default().context_window`。
    #[test]
    fn test_context_window_for_matches_current_and_falls_back() {
        let _guard = env_lock();
        let _env = clear_api_env();
        let config = Config::from_env().unwrap();
        assert_eq!(
            config.context_window_for(&config.model),
            config.context_window(),
            "按名查与按当前 model 查必须一致"
        );
        assert_eq!(
            config.context_window_for("definitely-not-a-real-model-xyz"),
            ys_component::RunLimits::default().context_window,
            "未知模型应回落默认窗口"
        );
    }

    #[test]
    fn test_config_model_factory() {
        let _guard = env_lock();
        let _env = clear_api_env();

        let mut config = Config::from_env().unwrap();
        assert!(config.build_model().is_none());

        // 用凭证设置 config
        config.api_base = Some("https://api.example.com".into());
        config.api_key = Some("sk-test".into());

        // 没有 factory 时仍返回 None
        assert!(config.build_model().is_none());

        // 注册 factory
        config.set_model_factory(|cfg| {
            if cfg.api_base.is_some() && cfg.api_key.is_some() {
                Some(Box::new(DummyModel))
            } else {
                None
            }
        });

        assert!(config.build_model().is_some());
    }

    #[test]
    fn test_config_current_compat() {
        // from_env 会读 API_ENV_KEYS，与改写这些 key 的测试共享同一把锁。
        let _guard = env_lock();
        let mut config = Config::from_env().unwrap();
        config.provider = Some("deepseek".into());
        let compat = config.current_compat();
        assert!(compat.has_reasoning_content);
    }

    #[test]
    fn test_config_current_compat_unknown() {
        let _guard = env_lock();
        let config = Config::from_env().unwrap();
        // 未设置 provider -> "custom" -> standard
        let compat = config.current_compat();
        assert!(!compat.has_reasoning_content);
        assert!(!compat.tool_calls_as_text);
    }

    /// `accepts_reasoning_effort` 是**模型级**判定：provider 能力位 + 该模型 id 的可解析性
    /// 两条门都要过。
    ///
    /// GLM 的判据是**能否把 id 识别为 GLM 模型**（解析出 `glm-<major>.<minor>`），而不是代际
    /// 是否够新：实测 5.2 及更早代际与 5.3 一样接受该字段，只是各自接受的档位集合不同
    /// （允许值见 `ProviderCompat::effort_for` 的折叠表）。解析不出代际的 id 一律
    /// fail-closed —— 非 GLM 模型收到该字段可能返回 400。
    ///
    /// 变异：把实现换成返回 `self.current_compat().supports_reasoning_effort` ⇒
    /// `glm` + 无法解析 id 那条断言变红（从 `false` 变 `true`）。
    #[test]
    fn test_accepts_reasoning_effort_is_model_level() {
        for (provider, model, expected) in [
            // provider 级能力位就不支持：模型名无关。
            ("deepseek", "deepseek-chat", false),
            ("deepseek", "glm-5.3", false),
            // `Verbatim` 方言没有模型级信息，信任能力位。
            ("opencode-go", "any-model", true),
            // GLM：能解析出代际就发（实测 5.2 及更早代际与 5.3 一样接受，只是档位集合不同）。
            ("glm", "glm-5.3", true),
            ("glm", "glm-5.3-flash", true),
            ("glm", "glm-5.2", true),
            // id 解析不出代际 ⇒ fail-closed。
            ("glm", "not-glm-at-all", false),
        ] {
            let mut config = test_config(model);
            config.provider = Some(provider.into());
            assert_eq!(
                config.accepts_reasoning_effort(),
                expected,
                "provider = {provider}, model = {model}"
            );
        }
    }

    /// 同一 provider 上 `ProviderCompat::supports_reasoning_effort` 与 `accepts_reasoning_effort`
    /// **必须可分**：若两者总相等，模型级门控就白做了。
    ///
    /// 可分的形态是「provider 认字段，但模型 id 解析不出 GLM 代际」：GLM 的能力位为 `true`，
    /// 而解析失败一律 fail-closed。代际新旧**不构成**差异 —— 实测 5.2 及更早代际也接受该字段。
    #[test]
    fn test_accepts_reasoning_effort_differs_from_provider_level() {
        let mut config = test_config("some-unknown-id");
        config.provider = Some("glm".into());
        assert!(
            config.current_compat().supports_reasoning_effort,
            "provider 级：GLM 端点接受该字段"
        );
        assert!(
            !config.accepts_reasoning_effort(),
            "模型级：id 解析不出 GLM 代际 ⇒ fail-closed"
        );
    }

    /// `request_headers` 跟着 `provider` / `api_base` 走：opencode-go 需要会话头，
    /// 其余 provider 不需要 —— 给别家多带一个无谓的头只会污染请求。
    #[test]
    fn test_config_request_headers_track_provider_and_base() {
        let mut config = test_config("deepseek-chat");
        assert!(config.request_headers().is_empty(), "未登录时无额外头");

        config.provider = Some("opencode-go".into());
        config.api_base = Some("https://opencode.ai/zen/go/v1".into());
        let headers = config.request_headers();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, crate::provider::OPENCODE_SESSION_HEADER);

        // env 直连：`provider` 缺席，仅凭 api_base 也得带上。
        config.provider = None;
        assert_eq!(config.request_headers().len(), 1);

        config.api_base = Some("https://api.deepseek.com".into());
        assert!(config.request_headers().is_empty());
    }

    #[test]
    fn test_startup_recovery_from_auth() {
        let _guard = env_lock();

        // 准备一个含 deepseek 凭证的临时 auth.json
        let dir = std::env::temp_dir().join("yushan_test_startup_recovery");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let auth_path = dir.join("auth.json");

        let auth_data = serde_json::json!({
            "deepseek": {
                "api_base": "https://api.deepseek.com",
                "api_key": "sk-test-recovery",
                "model": "deepseek-chat"
            }
        });
        std::fs::write(
            &auth_path,
            serde_json::to_string_pretty(&auth_data).unwrap(),
        )
        .unwrap();

        // 用无环境变量（未配置）的方式创建 Config
        let _env = clear_api_env();

        let mut config = Config::from_env().unwrap();
        config.registry.set_auth_override(auth_path);
        config.registry.load_auth();

        // 模拟 main.rs 的恢复循环
        if !config.is_configured() {
            for provider in config.registry.providers() {
                if let Some(entry) = config.registry.auth_for(&provider.name) {
                    config.api_base = Some(entry.api_base.clone());
                    config.api_key = Some(entry.api_key.clone());
                    config.model = entry.model.clone();
                    config.provider = Some(provider.name.clone());
                    break;
                }
            }
        }

        assert!(config.is_configured());
        assert_eq!(config.api_base.as_deref(), Some("https://api.deepseek.com"));
        assert_eq!(config.api_key.as_deref(), Some("sk-test-recovery"));
        assert_eq!(config.model, "deepseek-chat");
        assert_eq!(config.provider.as_deref(), Some("deepseek"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- context_window 三级解析 + config.json ----

    /// **三级优先级逐级命中**（本刀的核心不变量）。
    ///
    /// 三个探针的选取都是「能区分相邻两级」的，不是随手挑的：
    /// - `MiniMax-M3` 内置 1M，config.json 覆盖 555_000 ⇒ **① 压过 ②**
    /// - `MiniMax-M2.7` 只在内置表（204_800 ≠ 1_000_000 默认）⇒ **② 压过 ③**
    /// - `unknown-model` 两处都无 ⇒ **③ 兜底 1_000_000**
    ///
    /// 变异自检：把 `context_window()` 里 ① ② 的顺序倒过来（内置表盖过 config.json），
    /// 第一条断言即变红。
    #[test]
    fn test_context_window_three_levels() {
        let (dir, path) = write_config(
            "three_levels",
            r#"{
                "context_window": { "MiniMax-M3": 555000, "deepseek-chat": 64000 }
            }"#,
        );

        let mut config = test_config("MiniMax-M3");
        config.load_user_config(&path);
        assert_eq!(
            config.context_window(),
            555_000,
            "① config.json 覆盖应压过 ② 内置表"
        );

        config.model = "MiniMax-M2.7".into();
        assert_eq!(config.context_window(), 204_800, "② 内置表应压过 ③ 默认值");

        config.model = "unknown-model".into();
        assert_eq!(
            config.context_window(),
            1_000_000,
            "③ 未知 model 回退 RunLimits 默认"
        );

        // 同一份文件里 deepseek-chat 也被覆盖（证明是**按 model 查表**，
        // 不是「文件里存在任何覆盖就全局生效」）
        config.model = "deepseek-chat".into();
        assert_eq!(config.context_window(), 64_000);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 窗口值与来源**成对**正确：四级各一条断言。
    ///
    /// 变异：把来源硬编码成「user override」（无论走到哪一级都返回
    /// [`ContextWindowSource::UserOverride`]）⇒ 下面 ② ③ ④ 三条断言同时变红。
    #[test]
    fn test_context_window_source_matches_each_level() {
        // ① 用户级 `config.json` 覆盖（最具体）。
        let (dir, path) = write_config(
            "window_source",
            r#"{
                "context_window": { "MiniMax-M3": 258400 }
            }"#,
        );
        let mut config = test_config("MiniMax-M3");
        config.load_user_config(&path);

        assert_eq!(
            config.context_window_with_source(),
            (258_400, ContextWindowSource::UserOverride),
            "① 用户覆盖：值与来源都要对"
        );

        // ② provider 目录自己报的窗口 —— 压过 ③ 内置表。
        config.registry.seed_cached_catalog(
            "https://api.minimax.chat/v1",
            &[ApiModel {
                id: "MiniMax-M2.7".into(),
                owned_by: None,
                name: None,
                context_window: Some(400_000),
            }],
        );
        config.model = "MiniMax-M2.7".into();
        assert_eq!(
            config.context_window_with_source(),
            (400_000, ContextWindowSource::ProviderCatalog),
            "② 目录应压过 ③ 内置表"
        );

        // ③ 内建兜底表（目录报不出窗口时）。
        config
            .registry
            .seed_cached_catalog("https://api.minimax.chat/v1", &[]);
        assert_eq!(
            config.context_window_with_source(),
            (204_800, ContextWindowSource::BuiltinTable),
            "③ 内置表：值与来源都要对"
        );

        // ④ 兜底默认值（谁都没报过）。
        config.model = "unknown-model-xyz".into();
        assert_eq!(
            config.context_window_with_source(),
            (
                ys_component::RunLimits::default().context_window,
                ContextWindowSource::Default
            ),
            "④ 未知 model 回退默认值"
        );

        // 窗口值与来源**必须出自同一次判定**：老接口拿到的值要和成对接口一致。
        for model in ["MiniMax-M3", "MiniMax-M2.7", "unknown-model-xyz"] {
            let (window, _) = config.context_window_with_source_for(model);
            assert_eq!(
                config.context_window_for(model),
                window,
                "窗口值与来源不得漂移：{model}"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件不存在 → 全默认，且**不 panic**（也不阻断启动）。
    #[test]
    fn test_context_window_missing_file_is_all_default() {
        let dir = temp_dir("missing_file");
        let path = dir.join("no_such_config.json");
        assert!(!path.exists());

        let mut config = test_config("deepseek-chat");
        config.load_user_config(&path);

        assert_eq!(config.context_window(), 1_000_000);
        assert_eq!(
            config.compact_policy(),
            ys_component::CompactPolicy::default()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **畸形 JSON 不 panic**：整份忽略，调用前状态原样保留。
    ///
    /// 变异：把 `load_user_config` 里的 `Err` 臂改成 `.unwrap()` → panic，本测试变红。
    #[test]
    fn test_malformed_json_is_ignored_without_panic() {
        let (dir, path) = write_config("malformed", "{ this is not json,,, }");

        let mut config = test_config("MiniMax-M3");
        config.load_user_config(&path);

        // 整份回退：内置表仍然生效（说明只是「用户覆盖没读到」，不是崩掉）
        assert_eq!(config.context_window(), 1_000_000);
        assert_eq!(config.compact_policy().trigger_ratio, 0.85);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空文件 / 顶层是数组（形状不对）同样只落日志、不 panic。
    #[test]
    fn test_wrong_shaped_json_is_ignored() {
        for (name, content) in [
            ("empty", ""),
            ("array", "[1, 2, 3]"),
            ("scalar", "42"),
            ("null", "null"),
            // context_window 是字符串而非对象
            ("bad_section", r#"{"context_window": "128000"}"#),
        ] {
            let (dir, path) = write_config(name, content);
            let mut config = test_config("MiniMax-M3");
            config.load_user_config(&path);
            assert_eq!(config.context_window(), 1_000_000, "case {name} 应整份回退");
            assert_eq!(
                config.compact_policy(),
                ys_component::CompactPolicy::default(),
                "case {name}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// **值非法被忽略**，且只丢非法那一条 —— 同文件里的合法条目照常生效。
    #[test]
    fn test_invalid_entries_are_dropped_individually() {
        let (dir, path) = write_config(
            "invalid_values",
            r#"{
                "context_window": {
                    "good-model": 200000,
                    "negative": -5,
                    "zero": 0,
                    "string": "128000",
                    "float": 1.5,
                    "bool": true,
                    "null": null,
                    "object": {}
                }
            }"#,
        );

        let mut config = test_config("good-model");
        config.load_user_config(&path);

        assert_eq!(config.context_window(), 200_000, "合法条目必须留下");
        for bad in [
            "negative", "zero", "string", "float", "bool", "null", "object",
        ] {
            config.model = bad.into();
            assert_eq!(
                config.context_window(),
                1_000_000,
                "非法条目 {bad} 应被丢弃（回退默认），而非生效"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `compaction` 只给 `trigger_ratio` 时，`keep_ratio` 仍为默认 0.10
    /// （两个比例**逐字段独立**覆盖）。
    #[test]
    fn test_compaction_partial_override_keeps_other_defaults() {
        let (dir, path) = write_config(
            "compaction_partial",
            r#"{"compaction": {"trigger_ratio": 0.70}}"#,
        );

        let mut config = test_config("deepseek-chat");
        config.load_user_config(&path);
        let policy = config.compact_policy();

        assert_eq!(policy.trigger_ratio, 0.70);
        assert_eq!(policy.keep_ratio, 0.10, "未给的比例必须保持默认");

        // 再补一份只给 keep_ratio 的，验证对称方向
        let (dir2, path2) = write_config(
            "compaction_partial_keep",
            r#"{"compaction": {"keep_ratio": 0.05}}"#,
        );
        let mut config2 = test_config("deepseek-chat");
        config2.load_user_config(&path2);
        let policy2 = config2.compact_policy();
        assert_eq!(policy2.trigger_ratio, 0.85, "未给的比例必须保持默认");
        assert_eq!(policy2.keep_ratio, 0.05);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// 比例越界 / 类型不对 → 落日志后忽略，回退默认值（不得 panic、不得生效）。
    #[test]
    fn test_invalid_ratios_are_ignored() {
        for (name, content) in [
            ("ratio_zero", r#"{"compaction": {"trigger_ratio": 0}}"#),
            (
                "ratio_negative",
                r#"{"compaction": {"trigger_ratio": -0.5}}"#,
            ),
            ("ratio_above_one", r#"{"compaction": {"keep_ratio": 1.5}}"#),
            ("ratio_string", r#"{"compaction": {"keep_ratio": "0.5"}}"#),
            ("ratio_null", r#"{"compaction": {"keep_ratio": null}}"#),
        ] {
            let (dir, path) = write_config(name, content);
            let mut config = test_config("deepseek-chat");
            config.load_user_config(&path);
            assert_eq!(
                config.compact_policy(),
                ys_component::CompactPolicy::default(),
                "case {name} 应回退默认策略"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// 边界合法值：`trigger_ratio = 1.0`（用满窗口才压缩）是合法意图，不得被丢弃。
    #[test]
    fn test_ratio_one_is_accepted() {
        let (dir, path) = write_config("ratio_one", r#"{"compaction": {"trigger_ratio": 1.0}}"#);
        let mut config = test_config("deepseek-chat");
        config.load_user_config(&path);
        assert_eq!(config.compact_policy().trigger_ratio, 1.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 未知字段被忽略（向后兼容：将来加字段不破坏旧 app）。
    #[test]
    fn test_unknown_fields_are_ignored() {
        let (dir, path) = write_config(
            "unknown_fields",
            r#"{
                "context_window": {"deepseek-chat": 64000},
                "future_section": {"whatever": 1},
                "top_level": 42
            }"#,
        );
        let mut config = test_config("deepseek-chat");
        config.load_user_config(&path);
        assert_eq!(config.context_window(), 64_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 重载语义：第二次 `load_user_config` **替换**（而非叠加）上一次的覆盖。
    #[test]
    fn test_reload_replaces_previous_overrides() {
        let (dir_a, path_a) = write_config(
            "reload_a",
            r#"{"context_window": {"deepseek-chat": 64000}}"#,
        );
        let (dir_b, path_b) = write_config("reload_b", r#"{"context_window": {}}"#);

        let mut config = test_config("deepseek-chat");
        config.load_user_config(&path_a);
        assert_eq!(config.context_window(), 64_000);

        config.load_user_config(&path_b);
        assert_eq!(
            config.context_window(),
            1_000_000,
            "第二次加载必须整体替换，旧覆盖不得残留"
        );

        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }

    /// 默认路径是 `~/.yushan/config.json`（**不是** `state.json` —— 那是 app 会覆盖的文件）。
    ///
    /// 本测试只读 `HOME`，但别处的测试会在 `env_lock` 下改写 / 移除它
    /// （`logging.rs`、`prompt.rs`、`main.rs`），故这里也必须持锁才能读到稳定值。
    #[test]
    fn test_default_user_config_path_shape() {
        let _guard = env_lock();
        let path = Config::default_user_config_path().expect("测试环境应有 HOME");
        assert!(path.ends_with(".yushan/config.json"), "{path:?}");
        assert!(!path.ends_with("state.json"));
        assert!(!path.ends_with("auth.json"));
    }

    /// 解析层与 `state.json` 的隔离：`load_user_config` 绝不写盘。
    #[test]
    fn test_load_user_config_does_not_write() {
        let (dir, path) = write_config(
            "readonly",
            r#"{"context_window": {"deepseek-chat": 64000}}"#,
        );
        let before = std::fs::read_to_string(&path).unwrap();
        let entries_before = std::fs::read_dir(&dir).unwrap().count();

        let mut config = test_config("deepseek-chat");
        config.load_user_config(&path);

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "文件不得被改写"
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            entries_before,
            "不得在目录里多出 state.json 之类的文件"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ───────────────────────── skills 装配（M4） ─────────────────────────

    /// 在 `root/<command>/SKILL.md` 落一份 skill。
    fn write_skill(root: &std::path::Path, command: &str, content: &str) {
        let dir = root.join(command);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    /// `from_env` 起步时**零 skill**（扫描是启动期显式的一步，不是构造副作用）；
    /// 空目录 / 不存在的根 ⇒ 零 skill、零诊断，且**其余字段不动**。
    ///
    /// 变异：把 `from_env` 改成顺带扫描真实 `~/.yushan/skills` ⇒ 第一组断言在开发机
    /// 上有 skill 时变红（且启动期会变成隐式 IO）。
    #[test]
    fn skills_start_empty_and_empty_roots_are_silent() {
        let _guard = env_lock();
        let _env = clear_api_env();

        let mut config = Config::from_env().unwrap();
        assert!(config.skills.is_empty(), "构造时不该有 skill");
        assert_eq!(config.skills_omitted, 0);

        let dir = temp_dir("skills_empty");
        let missing = dir.join("no_such_root");
        let model_before = config.model.clone();

        let diags = config.load_skills(&[missing, dir.clone()]);
        assert!(diags.is_empty(), "空根不产诊断：{diags:?}");
        assert!(config.skills.is_empty());
        assert_eq!(config.skills_omitted, 0);
        assert_eq!(config.model, model_before, "load_skills 不得碰别的字段");
        assert_eq!(config.cwd, std::env::current_dir().unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `load_skills` 按 roots 顺序取条目，**同名时 project 胜**（`roots[0]` = project）——
    /// 这是 [`crate::skills::scan`] 的 scope-by-position 契约在装配路径上的端到端钉法。
    ///
    /// ⚠️ 本用例**显式传 roots**（`[project, user]`），故它守的是 `scan` 的「位置即
    /// scope」契约，**不是** [`Config::default_skill_roots`] 的顺序 —— 反转后者**不会**
    /// 让本用例变红（它根本没调那个函数）。生产侧顺序的真正守卫是
    /// [`default_skill_roots_are_project_first_and_never_claude`]。
    ///
    /// 变异：把本用例里传的 roots 对调（user 在前）⇒ 本用例变红（胜者变成用户级正文）。
    #[test]
    fn load_skills_respects_root_order_project_wins() {
        let _guard = env_lock();
        let _env = clear_api_env();

        let project = temp_dir("skills_proj");
        let user = temp_dir("skills_user");
        write_skill(
            &project,
            "git-status",
            "---\nname: 项目版\ndescription: 项目级描述\n---\n项目正文\n",
        );
        write_skill(
            &user,
            "git-status",
            "---\nname: 用户版\ndescription: 用户级描述\n---\n用户正文\n",
        );

        let mut config = Config::from_env().unwrap();
        // **project 在前** —— 与 `default_skill_roots` 同序。
        let diags = config.load_skills(&[project.clone(), user.clone()]);

        assert_eq!(config.skills.len(), 1, "同名只留一条");
        assert_eq!(config.skills[0].description, "项目级描述");
        assert_eq!(config.skills[0].path, project.join("git-status/SKILL.md"));
        assert_eq!(diags.len(), 1, "遮蔽恰好一条诊断：{diags:?}");

        let _ = std::fs::remove_dir_all(&project);
        let _ = std::fs::remove_dir_all(&user);
    }

    /// **`default_skill_roots` 的顺序是语义**：第一条**必须**是 `<cwd>/.yushan/skills`
    /// （`scan` 靠位置判 project），第二条才是 `~/.yushan/skills`；且**不扫 `.claude`**。
    ///
    /// 变异：两条对调 ⇒ `roots[0]` 断言变红 —— 那会让用户级静默遮蔽项目级
    /// （与 ADR-0019 决策 6 相反），是**提示词缓存与同名遮蔽语义的真值**。
    #[test]
    fn default_skill_roots_are_project_first_and_never_claude() {
        let _guard = env_lock();
        let _env = EnvRestore::capture(&["HOME", "USERPROFILE"]);
        // SAFETY: 调用方持有 env_lock，独占这两个 key 的修改与恢复。
        unsafe {
            std::env::set_var("HOME", "/tmp/yushan-skills-home-probe");
            std::env::remove_var("USERPROFILE");
        }

        let cwd = std::path::Path::new("/work/proj");
        let roots = Config::default_skill_roots(cwd);
        assert_eq!(roots.len(), 2, "有 HOME 时两条根：{roots:?}");
        assert_eq!(
            roots[0],
            PathBuf::from("/work/proj/.yushan/skills"),
            "第一条必须是项目级（`scan` 靠位置判 project）"
        );
        assert_eq!(
            roots[1],
            PathBuf::from("/tmp/yushan-skills-home-probe/.yushan/skills"),
            "第二条才是用户级"
        );
        assert!(
            roots
                .iter()
                .all(|r| !r.to_string_lossy().contains(".claude")),
            "不扫 `.claude/skills`（设计 D2）：{roots:?}"
        );

        // 无 HOME / USERPROFILE ⇒ 只剩项目那一条（不得凭空造出相对路径根）。
        // SAFETY: 同上，仍持锁。
        unsafe {
            std::env::remove_var("HOME");
        }
        assert_eq!(
            Config::default_skill_roots(cwd),
            vec![PathBuf::from("/work/proj/.yushan/skills")]
        );
    }

    /// **`cwd == $HOME` 时去重**：两条根是同一路径 ⇒ 只返回项目那一条。
    ///
    /// 不去重的话 `scan` 会把每个 skill 收两遍（一条 `Project`、一条 `User`，`path`
    /// 相同），`shadow_diagnostics` 便给每个 skill 产一条**假的**「project 级遮蔽
    /// user 级」诊断 —— 实测 56 条语料 ⇒ 56 行噪音，淹没 R4 依赖的真信号，启动期
    /// IO 还翻倍。
    ///
    /// 变异：去掉 `default_skill_roots` 里的 `if user != project` 判据 ⇒ 本用例
    /// 第一条断言变红（返回两条同路径的根）。
    #[test]
    fn default_skill_roots_dedup_when_cwd_is_home() {
        let _guard = env_lock();
        let home = "/tmp/yushan-skills-home-dedup";
        let _env = EnvRestore::capture(&["HOME", "USERPROFILE"]);
        // SAFETY: 调用方持有 env_lock，独占这两个 key 的修改与恢复。
        unsafe {
            std::env::set_var("HOME", home);
            std::env::remove_var("USERPROFILE");
        }

        // cwd == $HOME ⇒ 两条根同一路径，去重成一条。
        let roots = Config::default_skill_roots(std::path::Path::new(home));
        assert_eq!(
            roots,
            vec![PathBuf::from(format!("{home}/.yushan/skills"))],
            "cwd == HOME 时只应有一条根（去重）：{roots:?}"
        );

        // cwd != $HOME ⇒ 仍是两条，且 project 在前（顺序语义不变）。
        let roots = Config::default_skill_roots(std::path::Path::new("/work/proj"));
        assert_eq!(roots.len(), 2, "cwd != HOME 时两条根：{roots:?}");
        assert_eq!(
            roots[0],
            PathBuf::from("/work/proj/.yushan/skills"),
            "project 仍必须在第一条"
        );
        assert_eq!(roots[1], PathBuf::from(format!("{home}/.yushan/skills")));
    }

    // ───── 长程完成配置：max_rounds / 时长 / 重试 / 超时（ADR-0027 / 0028） ─────
    //
    // 这些用例直接调 `parse_user_config`（传 JSON 字符串），**不碰文件系统**：
    // 校验 / 丢弃语义全在解析层，无需为「只丢该条」造临时文件。

    /// `max_rounds`：合法正整数被采纳；缺省回落 `u32::MAX`（= 不设）。
    ///
    /// 变异：把调用点的 `min` 从 `1u32` 改成 `0u32`（即接受 0）⇒
    /// [`test_max_rounds_invalid_dropped_individually`] 的 `zero` 用例变红。
    #[test]
    fn test_max_rounds_adopted_and_default_is_unset() {
        let mut config = test_config("deepseek-chat");
        assert_eq!(config.max_rounds(), u32::MAX, "缺省 = 不设");
        assert_ne!(
            config.max_rounds(),
            ys_component::RunLimits::default().max_rounds,
            "app 产品默认必须与库的保守默认（10）分离（防恒真）"
        );

        config.user_config = parse_user_config(r#"{"max_rounds": 42}"#).unwrap();
        assert_eq!(config.max_rounds(), 42, "合法正整数应被采纳");
    }

    /// `max_rounds` 非法值**逐条丢弃**，且**只丢该条**（同文件其他项照常生效）。
    ///
    /// 变异：`min` 判据从 `>= 1` 放开到 `>= 0` ⇒ `zero` 用例变红。
    #[test]
    fn test_max_rounds_invalid_dropped_individually() {
        for (name, value) in [
            ("zero", "0"),
            ("negative", "-3"),
            ("float", "1.5"),
            ("string", "\"100\""),
            ("null", "null"),
            ("bool", "true"),
            ("object", "{}"),
            ("array", "[1, 2]"),
        ] {
            let json = format!(r#"{{"max_rounds": {value}}}"#);
            let user = parse_user_config(&json).unwrap();
            assert_eq!(user.max_rounds, None, "case {name} 应被丢弃");
        }

        // 只丢该条：非法 `max_rounds` 不影响同一份文件里的 `context_window`。
        let user =
            parse_user_config(r#"{"max_rounds": 0, "context_window": {"deepseek-chat": 64000}}"#)
                .unwrap();
        assert_eq!(user.max_rounds, None, "非法 max_rounds 应被丢弃");
        assert_eq!(
            user.windows.get("deepseek-chat"),
            Some(&64_000),
            "同文件其他项必须照常生效"
        );
    }

    /// `max_turn_duration_secs`：合法正整数被采纳；缺省 = 48h；`0` / 非法类型回落默认。
    #[test]
    fn test_max_turn_duration_adopted_and_default() {
        let mut config = test_config("deepseek-chat");
        assert_eq!(
            config.max_turn_duration(),
            std::time::Duration::from_secs(172_800),
            "缺省 = 48h"
        );

        config.user_config = parse_user_config(r#"{"max_turn_duration_secs": 3600}"#).unwrap();
        assert_eq!(
            config.max_turn_duration(),
            std::time::Duration::from_secs(3600)
        );

        for (name, value) in [
            ("zero", "0"),
            ("negative", "-1"),
            ("float", "1.5"),
            ("string", "\"3600\""),
            ("null", "null"),
        ] {
            let json = format!(r#"{{"max_turn_duration_secs": {value}}}"#);
            let user = parse_user_config(&json).unwrap();
            assert_eq!(user.max_turn_duration_secs, None, "case {name} 应被丢弃");
        }
    }

    /// `model_retry` **逐字段独立**覆盖：只给一项时另两项仍是默认；`max_attempts: 0` 合法。
    ///
    /// 变异：把逐字段独立改成「三项任一存在就整体覆盖」⇒「只给 base_delay_ms」那组
    /// 断言变红（未给的字段会被写成 0，而非保持默认）。
    #[test]
    fn test_model_retry_independent_fields() {
        // 缺省全默认。
        let config = test_config("deepseek-chat");
        assert_eq!(config.model_retry(), ModelRetryConfig::default());
        assert_eq!(
            config.model_retry(),
            ModelRetryConfig {
                max_attempts: 5,
                base_delay_ms: 500,
                max_delay_ms: 30_000,
            }
        );

        // 只给 base_delay_ms ⇒ 另两项保持默认。
        let user = parse_user_config(r#"{"model_retry": {"base_delay_ms": 1000}}"#).unwrap();
        assert_eq!(
            user.model_retry,
            ModelRetryConfig {
                max_attempts: 5,
                base_delay_ms: 1000,
                max_delay_ms: 30_000,
            }
        );

        // `max_attempts: 0`（不重试）是**合法**值；另两项仍默认。
        let user = parse_user_config(r#"{"model_retry": {"max_attempts": 0}}"#).unwrap();
        assert_eq!(
            user.model_retry,
            ModelRetryConfig {
                max_attempts: 0,
                base_delay_ms: 500,
                max_delay_ms: 30_000,
            }
        );

        // 三项都给 ⇒ 三项全采纳。
        let user = parse_user_config(
            r#"{"model_retry": {"max_attempts": 2, "base_delay_ms": 250, "max_delay_ms": 8000}}"#,
        )
        .unwrap();
        assert_eq!(
            user.model_retry,
            ModelRetryConfig {
                max_attempts: 2,
                base_delay_ms: 250,
                max_delay_ms: 8000,
            }
        );
    }

    /// `model_retry` 非法字段被丢弃 ⇒ **该字段**回落默认（不 panic、不整体覆盖）。
    #[test]
    fn test_model_retry_invalid_fields_fall_back() {
        for (name, json) in [
            ("negative", r#"{"model_retry": {"base_delay_ms": -5}}"#),
            ("string", r#"{"model_retry": {"max_attempts": "3"}}"#),
            ("object", r#"{"model_retry": {"max_delay_ms": {}}}"#),
        ] {
            let user = parse_user_config(json).unwrap();
            assert_eq!(
                user.model_retry,
                ModelRetryConfig::default(),
                "case {name} 非法字段应回落默认"
            );
        }
    }

    /// `model_timeout` 逐字段独立；缺省 15 / 120；`0` 无意义 ⇒ 丢弃、回落默认。
    #[test]
    fn test_model_timeout_independent_fields() {
        let config = test_config("deepseek-chat");
        assert_eq!(config.model_timeout(), ModelTimeoutConfig::default());
        assert_eq!(
            config.model_timeout(),
            ModelTimeoutConfig {
                connect_secs: 15,
                idle_secs: 120,
            }
        );

        let user = parse_user_config(r#"{"model_timeout": {"connect_secs": 30}}"#).unwrap();
        assert_eq!(
            user.model_timeout,
            ModelTimeoutConfig {
                connect_secs: 30,
                idle_secs: 120,
            },
            "只给 connect_secs 时 idle_secs 保持默认"
        );

        let user = parse_user_config(r#"{"model_timeout": {"idle_secs": 300}}"#).unwrap();
        assert_eq!(
            user.model_timeout,
            ModelTimeoutConfig {
                connect_secs: 15,
                idle_secs: 300,
            },
            "只给 idle_secs 时 connect_secs 保持默认"
        );

        // 0 无意义（超时为 0 会立刻超时）⇒ 丢该字段、回落默认。
        for (name, json) in [
            ("connect_zero", r#"{"model_timeout": {"connect_secs": 0}}"#),
            ("idle_zero", r#"{"model_timeout": {"idle_secs": 0}}"#),
        ] {
            let user = parse_user_config(json).unwrap();
            assert_eq!(
                user.model_timeout,
                ModelTimeoutConfig::default(),
                "case {name} 应回落默认"
            );
        }
    }
}
