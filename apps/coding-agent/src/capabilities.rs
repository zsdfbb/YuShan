//! 命令的**能力实现**（设计 §6「能力实现归 app」）。
//!
//! 这里没有 `Command` trait、没有 registry、没有 `CommandContext` —— 命令的
//! **用户可见行为**（提示什么、问什么、什么时候开浮层）全在 TUI crate
//! (`ys-tui-coding`) 里，本模块只做「收到一条 [`Request`] 之后，世界要怎么变」。
//!
//! ```text
//! UI: 输入 → parse → ┬─ Local（TUI 自己处理）
//!                    └─ Request ──信道①──► app_loop ──► 本模块的函数
//! ```
//!
//! [`Request`]: ys_protocol::Request
//!
//! # 输出契约
//!
//! 每个函数返回 `Vec<String>`（每项一行），由 `app_loop` 包成
//! [`Outbound::Output`](ys_protocol::Outbound::Output) 送进 transcript ——
//! **本模块绝不 print**：TUI 起屏后 stderr/stdout 会冲掉整屏（设计 §5）。
//! 命令期间的失败（凭证/状态落盘失败、无法构建 model）也走同一条路。

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use ys_core::Message;
use ys_model::Model;
use ys_protocol::SessionSummary;
use ys_session::SessionMeta;

use crate::config::Config;
use crate::provider::{AuthEntry, ProviderRegistry};
use crate::state::{AppState, StateStore};
use crate::wiring::Wiring;

/// `/login` 的能力：按 provider 名 + api_key 登录，存 auth.json，重建 model。
///
/// # api_base 的来路（三级回退）
///
/// 1. **用户显式给的** `requested_api_base`（`/login custom` 时 UI 追问到的 URL）
/// 2. provider 目录里的 `api_base`（deepseek / minimax 有值）
/// 3. 回退 `config.api_base`（通常来自 `YUSHAN_API_BASE`）
/// 4. 都为空 → 返回一条**明确的**提示（**不静默失败**）
///
/// 「问什么」归 UI（[`ys_tui_coding::prompter::resolve_prompt`] 只对**没有内置
/// base** 的 provider 追问 URL），本函数只负责按上面的顺序解析 —— 空串一律视同
/// 未提供（不能拿一个空 base 覆盖 config 里已有的值）。
///
/// [`Request::Login`]: ys_protocol::Request::Login
pub fn login(
    config: &mut Config,
    wiring: &mut Wiring,
    state: &StateStore,
    provider_name: &str,
    api_key: &str,
    requested_api_base: Option<&str>,
) -> Vec<String> {
    let Some(provider) = config.registry.find_provider(provider_name).cloned() else {
        let names: Vec<String> = config
            .registry
            .providers()
            .iter()
            .map(|p| p.name.clone())
            .collect();
        return vec![format!(
            "Unknown provider: {provider_name}. Available: {}",
            names.join(", ")
        )];
    };

    let api_base = requested_api_base
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| Some(provider.api_base.clone()).filter(|b| !b.is_empty()))
        .or_else(|| config.api_base.clone().filter(|b| !b.is_empty()));

    let Some(api_base) = api_base else {
        return vec![
            format!("Provider `{provider_name}` has no API base."),
            "Please provide the API base URL when logging in, or set YUSHAN_API_BASE and retry (deepseek / minimax have built-in bases).".to_string(),
        ];
    };

    let model_name = if provider.default_model.is_empty() {
        config.model.clone()
    } else {
        provider.default_model.clone()
    };

    let mut out = Vec::new();

    // 持久化凭证（失败不致命：本次会话照样能用，只是下次启动恢复不到）。
    if let Err(e) = config.registry.save_auth(
        provider_name,
        &AuthEntry {
            api_base: api_base.clone(),
            api_key: api_key.to_string(),
            model: model_name.clone(),
        },
    ) {
        out.push(format!("Warning: Could not persist credentials: {e}"));
    }

    config.api_base = Some(api_base);
    config.api_key = Some(api_key.to_string());
    config.model = model_name.clone();
    config.provider = Some(provider_name.to_string());

    // 经 factory 重建 model 交给接线器（ADR-0010：配置归接线器）。
    match config.build_model() {
        Some(model) => wiring.set_model(Some(model)),
        None => out.push("Warning: Could not build model. Check API credentials.".to_string()),
    }

    if let Err(e) = state.save(&AppState {
        last_active_provider: Some(provider_name.to_string()),
        last_active_model: Some(model_name.clone()),
    }) {
        out.push(format!("Warning: Could not persist state: {e}"));
    }

    out.push(format!("✓ Logged in to {provider_name} ({model_name})."));
    out
}

/// `/logout`：清空当前 provider 的凭证 + config 字段 + 接线器的 model。
pub fn logout(config: &mut Config, wiring: &mut Wiring, state: &StateStore) -> Vec<String> {
    let mut out = Vec::new();

    if let Some(name) = config.provider.clone()
        && let Err(e) = config.registry.remove_auth(&name)
    {
        out.push(format!(
            "Warning: Could not remove persisted credentials: {e}"
        ));
    }

    config.api_base = None;
    config.api_key = None;
    config.provider = None;
    // 配置归接线器，不伸手进 agent（ADR-0010）。
    wiring.set_model(None);

    if let Err(e) = state.save(&AppState::default()) {
        out.push(format!("Warning: Could not persist state: {e}"));
    }

    out.push("Logged out. API credentials cleared.".to_string());
    out
}

/// `/model <name>`：切模型并重建（经 factory）。带参数的那条路径 —— 无参的
/// 「浮层选一个」在 TUI 里，选完照样发 [`Request::SetModel`]。
///
/// **跨 provider 选模型**：候选是所有已登录 provider 的并集，选中别家的模型时必须连
/// 端点一起切 —— 只改 `config.model` 会把 A 家的模型名发到 B 家的 base（换来一个 400）。
///
/// # 归属从哪来（`provider` 形参）
///
/// 三条来源，**优先级从高到低**：
///
/// 1. **`provider` 形参**（浮层选中时由 UI 带上）—— 权威，直接采用；
/// 2. **限定名** `provider/model`（手敲）—— 仅当 `/` 前的部分**逐字命中一个已知
///    provider 名**时才拆（见 [`ProviderRegistry::find_provider`]）。不这么做的话，
///    目录里带 `/` 的网关模型（OpenRouter 风格的 `vendor/model`）会被误切；
/// 3. **反查** [`ProviderRegistry::provider_for_model`]（裸模型名兜底）。
///
/// 为什么不让「反查」独自承担：它遍历 `model_cache`（`HashMap`，**迭代顺序不确定**）
/// 返回第一个含该 id 的 provider —— 两个 provider 的目录里出现**逐字同名**的 id 时，
/// 归属是任意的，会把端点静默切到错误的一家。1/2 都把归属**显式带过来**，绕开这个猜测。
///
/// 三条都没命中 ⇒ **保持当前 provider 不变**（既有语义：目录里都没有的手敲名字，
/// 只改 `config.model` 而不动端点）。
///
/// [`Request::SetModel`]: ys_protocol::Request::SetModel
/// [`ProviderRegistry::provider_for_model`]: crate::provider::ProviderRegistry::provider_for_model
/// [`ProviderRegistry::find_provider`]: crate::provider::ProviderRegistry::find_provider
pub fn set_model(
    config: &mut Config,
    wiring: &mut Wiring,
    state: &StateStore,
    model: &str,
    provider: Option<&str>,
) -> Vec<String> {
    let mut out = Vec::new();

    let (model, owner) = match provider {
        Some(p) => (model.to_string(), Some(p.to_string())),
        None => match split_qualified_model(model, &config.registry) {
            Some((p, bare)) => (bare, Some(p)),
            None => (
                model.to_string(),
                config.registry.provider_for_model(model),
            ),
        },
    };

    if let Some(owner) = owner
        && config.provider.as_deref() != Some(owner.as_str())
        && let Some(entry) = config.registry.auth_for(&owner).cloned()
    {
        let from = config
            .provider
            .clone()
            .unwrap_or_else(|| "(none)".to_string());
        config.api_base = Some(entry.api_base);
        config.api_key = Some(entry.api_key);
        config.provider = Some(owner.clone());
        out.push(format!("Provider switched: {from} → {owner}"));
    }

    config.model = model.clone();

    if let Err(e) = state.save(&AppState {
        last_active_provider: config.provider.clone(),
        last_active_model: Some(model.clone()),
    }) {
        out.push(format!("Warning: Could not persist state: {e}"));
    }

    // 重建失败（缺凭证）时把接线器的 model 一并置空：否则「用户刚切的模型」
    // 与「实际在跑的模型」会悄悄不一致（旧的 `/model` 只在 stdout 里提一句，
    // 但那时 model 已经不归 agent 持有，等价性无从谈起）。
    match config.build_model() {
        Some(built) => {
            wiring.set_model(Some(built));
            out.push(format!("Model switched to: {model}"));
        }
        None => {
            wiring.set_model(None);
            out.push(format!("Model set to: {model}"));
            out.push("Note: Cannot build model. Check API credentials with /login.".to_string());
        }
    }
    out
}

/// 拆 `/model` 的**限定名** `provider/model`：仅当 `/` 前那段逐字命中一个已知 provider
/// 名时才拆，返回 `(provider, 裸模型名)`；否则 `None`（整串当模型 id）。
///
/// **判据必须是「命中已知 provider 名」而不是「含 `/`」**：网关的模型 id 本身可以带
/// `/`（`vendor/model` 形态），见到 `/` 就拆会把它们误切成「provider=vendor」。
///
/// 变异：把 `registry.find_provider(prefix)?` 换成 `Some(())`（即任何 `/` 都拆）
/// ⇒ `test_set_model_slash_model_not_split_when_prefix_unknown` 变红。
fn split_qualified_model(model: &str, registry: &ProviderRegistry) -> Option<(String, String)> {
    let (prefix, rest) = model.split_once('/')?;
    if rest.is_empty() || registry.find_provider(prefix).is_none() {
        return None;
    }
    Some((prefix.to_string(), rest.to_string()))
}

/// `/new`：换会话（`Wiring::new_session`）。一次性会话返回 `None`。
pub async fn new_session(wiring: &mut Wiring) -> Vec<String> {
    match wiring.new_session().await {
        Ok(Some(path)) => vec![format!(
            "New conversation started. Session: {}",
            path.display()
        )],
        Ok(None) => vec!["New conversation started. Session cleared.".to_string()],
        Err(e) => vec![format!("Failed to start new session: {e}")],
    }
}

/// `/compact`：真压缩 —— 把手里的旧消息摘要成一条 checkpoint，尾部原样保留。
///
/// # 为什么不是「清空会话」
///
/// 改前是 MVP 占位：无脑 `wiring.clear_session()`。那等于**静默失忆** —— 用户以为
/// 上下文被压缩了，实际几十万 token 的历史被丢掉且不可挽回（会话文件也被截断）。
/// 现在四条路径里**三条失败**（无 model / 无事可压 / 摘要失败）**都不碰 session
/// 一个字节**，且各自给出一行明确提示 —— 靠「什么都不做」而不是「清空」来实现降级。
///
/// # 参数
///
/// `policy` 与 `window` 由 app 侧从 `config` 取（与 agent 读的是同一份配置）：
/// 手动 `/compact` **不等自动触发阈值** —— 用户点了就压。窗口参与的是
/// 「保留多少」（`policy.keep_budget(window)`），不是「是否触发」。
pub async fn compact(
    wiring: &mut Wiring,
    policy: &ys_component::CompactPolicy,
    window: usize,
) -> Vec<String> {
    // model 与会话必须**同时**借出：`Wiring::ports` 是唯一的同时借出口
    // （分开借会被借用检查器拦下 —— model 的不可变借用压着 session 的可变借用）。
    let ports = wiring.ports();
    let Some(model) = ports.model else {
        // 无 model ⇒ 明确提示并收手。**绝不**退回 `clear_session`：
        // 那会把「压不了」变成「历史没了」。
        return vec!["Cannot compact: no model configured. Use /login.".to_string()];
    };

    // `plan()` 是纯同步函数、**不碰 model**：消息太少（`len < 2`）或结构上无法
    // 同时保证「摘要段非空 + 保留段非空」时返回 `None`。
    let messages = ports.session.messages();
    let Some(plan) = ys_loop::plan(messages, policy, window) else {
        return vec!["Nothing to compact.".to_string()];
    };

    match ys_loop::execute(messages, &plan, model, policy).await {
        Ok(outcome) => {
            let before = outcome.estimated_tokens_before;
            let after = outcome.estimated_tokens_after;
            let summarized = outcome.summarized;
            let kept = outcome.kept;
            // 写回由**调用方**做（`execute` 不持有 session，评审 R3）——
            // 这正是「失败不丢历史」的结构保证。
            if let Err(e) = ports.session.replace_all(outcome.messages).await {
                return vec![format!("Compaction failed: {e}")];
            }
            vec![format!(
                "Compacted: ~{before} → ~{after} tokens ({summarized} summarized, {kept} kept)."
            )]
        }
        // B-3：`execute` 失败时它自己什么都没改 —— 这里**只**返回提示，
        // 不得「顺手」把会话清掉。
        Err(e) => vec![format!("Compaction failed: {e}")],
    }
}

/// `/export [path]`：把会话文件复制到目标路径。
///
/// `path` 为 `None` 时落点**由 app 决定**（设计 §6）—— 就用会话文件自身路径，
/// 即「确保会话已完整落盘」；这比让 TUI 猜一个相对 cwd 的路径更安全。
/// 一次性会话（`-p`/`--json` 之外的交互会话不会遇到，但保持一致）没有会话
/// 文件可导出，明确告知而不是静默 no-op。
///
/// **同步**（`std::fs::copy`）：会话是本地小文件，一次复制不值得为它把一个
/// `&Wiring`（非 `Sync` —— `Box<dyn Session>` 只保证 `Send`）借进 app 循环的
/// future 跨 `await`，那会让整个 app 循环不再 `Send`、无法 `spawn` 到 worker。
pub fn export(wiring: &Wiring, path: Option<PathBuf>) -> Vec<String> {
    let Some(source) = wiring.session_path().map(PathBuf::from) else {
        return vec!["Export: this session is not persisted — nothing to export.".to_string()];
    };
    let target = path.unwrap_or_else(|| source.clone());

    // **自拷贝护栏**：无参 `/export` 的落点就是会话文件本身，而 `fs::copy` 对
    // 「源 == 目标」会先把目标截断 —— 那不是导出，是把会话清空。目标与会话文件
    // 是同一个文件时，`/export` 的语义退化为「确认已落盘」。
    if is_same_file(&source, &target) {
        return vec![format!("Session already persisted at {}", source.display())];
    }

    match std::fs::copy(&source, &target) {
        Ok(bytes) => vec![format!(
            "Exported {} byte(s) to {}",
            bytes,
            target.display()
        )],
        Err(e) => vec![format!("Export failed: {e}")],
    }
}

/// 两个路径是否指向同一个文件（两边都能 canonicalize 时按真实路径比，
/// 否则退回字面比较 —— 目标还不存在时 canonicalize 会失败）。
fn is_same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(real_a), Ok(real_b)) => real_a == real_b,
        _ => a == b,
    }
}

/// 列出会话目录下的既有会话（`/resume` 无参的能力）。
///
/// **同步**调用 `sessions::list`（决策 2：照惯例在 app 线程同步调用，**不**引入
/// `spawn_blocking`）。上界论证沿 [`export`] 的先例：`list` 是对目录内每个
/// `*.jsonl` 的**一次 `stat` + 首行 4 KiB 读取**，条目数即用户自己的会话数
/// （数十量级），单次总成本在毫秒量级；把它包成 `spawn_blocking` 只会引入一次
/// 线程跨跳与一次 `&Path` 的所有权搬运，收益不抵复杂度。**不落盘、不发网络**。
pub fn list_sessions(dir: &Path) -> Vec<SessionSummary> {
    crate::sessions::list(dir)
}

/// 一次成功的运行期恢复 —— `app_loop` 据此完成提交步与出站。
///
/// 三样恰好对应 [`Outbound::SessionResumed`](ys_protocol::Outbound::SessionResumed)
/// 的载荷；**不放进 `ys-protocol`** —— 它是 app 侧的中间产物（`summary` 供
/// `app_loop` 组装，`thinking_visible` 还要用来重设进程级镜像）。
pub struct ResumedSession {
    pub summary: SessionSummary,
    /// 目标会话 meta 的 thinking 开关（`app_loop` 用它重设进程级镜像，见 I9）。
    pub thinking_visible: bool,
    /// 目标会话 meta 记录的工作树路径（`app_loop` 据此**重建 agent** 切工作区）。
    ///
    /// `None` = 该会话未绑定工作树（回共享工作区）。**绝不**回退成调用方 cwd
    /// —— 那会静默扩大可写根（Codex issue #4791 的教训）。
    pub workspace: Option<PathBuf>,
    /// 目标会话的完整历史（`app_loop` 随 `SessionResumed` 交给 UI 回放）。
    pub history: Vec<Message>,
}

/// `/resume` 的运行期恢复：把当前会话切到 `target`，并跟随目标 meta 的 thinking。
///
/// # 顺序即不变量（设计 §3.3）
///
/// 所有**可失败**步全部前置到「替换会话」之前：解析目标 → 读 meta → 纯解析模型
/// 四字段 → 构建候选 model。`wiring.resume_into` 一旦执行就已替换会话，故它之后
/// **只有**不可失败的提交步。任一步失败都只返回一条错误文案，**不改动**会话、
/// 模型、thinking、统计（I1）。这与启动期 `--resume` 的真实顺序严格对齐。
///
/// # 错误形态
///
/// 沿既有 `capabilities::*` 形态返回文本行（**失败不返回 `Err`**）；`None` 表示
/// 未切换。内部**不吞掉** `SessionError` 的分类（`open` 失败时把其 `Display`
/// 带进文案）。
///
/// # app 侧其余提交步归调用方
///
/// 进程级 thinking 镜像、`TurnStats` 播种、`session_started`、会话路径快照都是
/// `app_loop` 自己的运行期状态，由它在本函数成功后完成。
pub async fn resume_session(
    cfg: &mut Config,
    wiring: &mut Wiring,
    state: &StateStore,
    target: &str,
) -> (Vec<String>, Option<ResumedSession>) {
    // 1. 目录：内存会话无目录可扫（生产恒有）。
    let Some(dir) = wiring.sessions_dir_path().map(Path::to_path_buf) else {
        return (
            vec!["Cannot resume: this session is not persisted.".to_string()],
            None,
        );
    };
    // 2. 解析目标（id 或路径）—— 失败 ⇒ 只报错，状态未动。
    let path = match crate::sessions::resolve(&dir, target) {
        Ok(p) => p,
        Err(e) => return (vec![e], None),
    };
    // 3. 读目标 meta（只读首行）—— 复用启动期的读法。
    let meta = crate::sessions::read_meta(&path);
    // 4. **纯解析**模型四字段（不写 `cfg`）；无可用凭证 ⇒ 报错、状态未动。
    let saved = state.load();
    let Some(choice) = crate::resolve_model_from_meta(cfg, meta.as_ref(), Some(&saved)) else {
        return (
            vec!["Cannot resume: no usable API credentials for this session.".to_string()],
            None,
        );
    };
    // 5. 用**候选四字段**构建候选 model（不改动 `cfg` 的可见状态）—— 构建失败 ⇒ 报错。
    let Some(built) = build_candidate_model(cfg, &choice) else {
        return (
            vec!["Cannot resume: failed to build a model for this session.".to_string()],
            None,
        );
    };
    // 6. 替换会话（`open` 成功才替换）。
    let meta = match wiring.resume_into(path.clone()).await {
        Ok(m) => m,
        Err(e) => return (vec![format!("Failed to resume session: {e}")], None),
    };
    // 7. 提交（此后不得再有失败路径）：写回 `cfg` 四字段 + 交给接线器。
    cfg.provider = choice.provider;
    cfg.api_base = Some(choice.api_base);
    cfg.api_key = Some(choice.api_key);
    cfg.model = choice.model;
    wiring.set_model(Some(built));

    let summary = session_summary(&path, meta.as_ref());
    let thinking_visible = meta.as_ref().map(|m| m.thinking_visible).unwrap_or(false);
    let workspace = meta.as_ref().and_then(|m| m.workspace.clone());
    let history = wiring.session_messages().to_vec();
    (
        vec![format!("Resumed session: {}", summary.id)],
        Some(ResumedSession {
            summary,
            thinking_visible,
            workspace,
            history,
        }),
    )
}

/// 用**候选四字段**构建 model，但**不改动 `cfg` 对外可见的状态**。
///
/// `build_model()` 需要一份带候选凭证的 `&Config`，而 `Config` 持有不可克隆的
/// factory（`Box<dyn Fn>`），无法拷出一份「带候选字段的副本」。故就地换入候选四
/// 字段、**同步**构建、随即还原 —— 全过程在任一 `await` 之前完成，对调用方而言
/// `cfg` 从未改变（这是设计 §3.3「此步之前不得写 cfg」的落地：不留下可观测的写）。
///
/// # 前置条件：factory 不得 panic
///
/// 「换入 → 构建 → 还原」不是 RAII：还原步靠控制流顺序到达。`Config` 的 model
/// factory（`build_model()` 间接调用）若 panic，栈展开会**跳过**还原，把候选四字段
/// 留在 `cfg` 里（后续 `build_model()` 会用错凭证）。生产 factory 只做构造、不
/// panic；此处显式声明该契约，供改动者判断是否要改成 guard 式还原。
fn build_candidate_model(cfg: &mut Config, choice: &crate::ModelChoice) -> Option<Box<dyn Model>> {
    let prev_provider = cfg.provider.take();
    let prev_api_base = cfg.api_base.take();
    let prev_api_key = cfg.api_key.take();
    let prev_model = std::mem::replace(&mut cfg.model, choice.model.clone());
    cfg.provider = choice.provider.clone();
    cfg.api_base = Some(choice.api_base.clone());
    cfg.api_key = Some(choice.api_key.clone());
    let built = cfg.build_model();
    cfg.provider = prev_provider;
    cfg.api_base = prev_api_base;
    cfg.api_key = prev_api_key;
    cfg.model = prev_model;
    built
}

/// 由目标会话路径 + 其 meta 构造一条 [`SessionSummary`]（`sessions::list` 的
/// **单文件版**）。`title` 走 [`crate::sessions::derive_title`]；无 title 时复用
/// [`crate::sessions::DEFAULT_TITLE`] —— 与 `sessions::list` 对同一文件给出的
/// 标题**同源**，改缺省标题只需改一处。
pub(crate) fn session_summary(path: &Path, meta: Option<&SessionMeta>) -> SessionSummary {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(UNIX_EPOCH);
    let title = meta
        .and_then(|m| m.title.as_deref())
        .map(crate::sessions::derive_title)
        .unwrap_or_else(|| crate::sessions::DEFAULT_TITLE.to_string());
    SessionSummary {
        id,
        path: path.to_path_buf(),
        mtime,
        title,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ys_core::{ContentBlock, Message, Role};
    use ys_event::CollectingSink;
    use ys_model::{MockModel, Model, ModelError, ModelEventSink, ModelRequest, ModelResponse};

    /// 一条 `tokens` 量级的文本消息（估算器是「字节 / 4」，用整数倍长度让切分
    /// 边界可精确断言）。`tag` 用于区分消息 —— 断言「谁被摘要掉、谁被留下」。
    fn tagged_message(role: Role, tag: &str, tokens: usize) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text {
                text: format!("{tag}{}", "a".repeat(tokens * 4)),
            }],
        }
    }

    /// 往当前会话灌 `n` 条各 `tokens` 量级的消息（User / Assistant 交替）。
    async fn seed(wiring: &mut Wiring, n: usize, tokens: usize) {
        for i in 0..n {
            let role = if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            };
            wiring
                .ports()
                .session
                .append(tagged_message(role, &format!("m{i}"), tokens))
                .await
                .unwrap();
        }
    }

    /// 拼出一条消息里的全部文本块。
    fn text_of(msg: &Message) -> String {
        msg.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// 测试目录名：**进程内原子序号 + `process::id()`**（并行安全，血液教训）。
    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "yushan_capabilities_{tag}_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 测试 model：`build_model` 的产物，只需满足 `Model`。
    struct DummyModel;

    #[async_trait::async_trait]
    impl Model for DummyModel {
        fn model_id(&self) -> &str {
            "dummy"
        }
        async fn complete(
            &self,
            _request: ModelRequest,
            _sink: &mut dyn ModelEventSink,
        ) -> Result<ModelResponse, ModelError> {
            unimplemented!("test dummy")
        }
    }

    /// 指向临时 auth.json 的 Config（默认 provider 目录保持不变）。
    fn test_config() -> (Config, PathBuf) {
        // `from_env` 的 `getenv` 必须与 `config.rs` 里在 `env_lock` 下改写这些
        // key 的用例串行（getenv/setenv 跨线程并发即数据竞争）。本函数是同步的，
        // guard 随返回自然释放，恰好覆盖到 `from_env` 这一次读。
        let _guard = crate::test_env::env_lock();

        let dir = temp_dir("auth");
        let mut config = Config::from_env().unwrap();
        config.registry.set_auth_override(dir.join("auth.json"));
        config.set_model_factory(|cfg| {
            if cfg.api_base.is_some() && cfg.api_key.is_some() {
                Some(Box::new(DummyModel))
            } else {
                None
            }
        });
        (config, dir)
    }

    fn test_state() -> (StateStore, PathBuf) {
        let dir = temp_dir("state");
        let mut store = StateStore::new();
        store.set_override(dir.join("state.json"));
        (store, dir)
    }

    fn test_wiring() -> Wiring {
        Wiring::ephemeral(None, Box::new(CollectingSink::new()))
    }

    /// `/login` 成功路径：config 三个字段被填、auth 落盘、接线器拿到 model。
    #[test]
    fn test_login_success_sets_config_wiring_and_auth_file() {
        let (mut config, dir) = test_config();
        let (state, state_dir) = test_state();
        let mut wiring = test_wiring();
        assert!(wiring.model_id().is_none());

        let out = login(
            &mut config,
            &mut wiring,
            &state,
            "deepseek",
            "sk-test",
            None,
        );

        assert_eq!(config.provider.as_deref(), Some("deepseek"));
        assert_eq!(config.api_base.as_deref(), Some("https://api.deepseek.com"));
        assert_eq!(config.api_key.as_deref(), Some("sk-test"));
        assert_eq!(config.model, "deepseek-chat");
        assert_eq!(wiring.model_id(), Some("dummy"), "应经 factory 构建 model");
        assert!(
            out.iter().any(|l| l.contains("Logged in to deepseek")),
            "{out:?}"
        );

        // 凭证真的落盘了（新 registry 重读）
        let mut reloaded = crate::provider::ProviderRegistry::new();
        reloaded.set_auth_override(dir.join("auth.json"));
        reloaded.load_auth();
        let entry = reloaded.auth_for("deepseek").expect("应已落盘");
        assert_eq!(entry.api_key, "sk-test");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// 未知 provider：给出可用列表，**不改任何状态**。
    #[test]
    fn test_login_unknown_provider_reports_and_changes_nothing() {
        let (mut config, dir) = test_config();
        let (state, state_dir) = test_state();
        let mut wiring = test_wiring();

        let out = login(&mut config, &mut wiring, &state, "nope", "sk-test", None);

        assert_eq!(out.len(), 1);
        assert!(out[0].contains("Unknown provider: nope"), "{out:?}");
        assert!(out[0].contains("deepseek"), "应列出可用 provider：{out:?}");
        assert!(config.api_key.is_none(), "不得改 config");
        assert!(config.provider.is_none());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// `custom` provider 且 `config.api_base` 为空 → 明确提示设 `YUSHAN_API_BASE`
    /// （**不静默失败**，也不写空 base 的 auth）。
    #[test]
    fn test_login_custom_without_api_base_hints_env_var() {
        let (mut config, dir) = test_config();
        config.api_base = None;
        let (state, state_dir) = test_state();
        let mut wiring = test_wiring();

        let out = login(&mut config, &mut wiring, &state, "custom", "sk-test", None);

        assert!(
            out.iter().any(|l| l.contains("YUSHAN_API_BASE")),
            "应提示环境变量：{out:?}"
        );
        assert!(config.api_key.is_none(), "不得半途写入凭证");
        assert!(!dir.join("auth.json").exists(), "不得落盘半截凭证");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// `custom` provider 回退到 `config.api_base`（来自 `YUSHAN_API_BASE`）。
    #[test]
    fn test_login_custom_falls_back_to_config_api_base() {
        let (mut config, dir) = test_config();
        config.api_base = Some("https://custom.example.com".into());
        let (state, state_dir) = test_state();
        let mut wiring = test_wiring();

        let out = login(&mut config, &mut wiring, &state, "custom", "sk-test", None);

        assert_eq!(
            config.api_base.as_deref(),
            Some("https://custom.example.com")
        );
        assert_eq!(wiring.model_id(), Some("dummy"));
        assert!(out.iter().any(|l| l.contains("✓ Logged in")), "{out:?}");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// **回归**：`custom` provider 由 UI 追问到的 URL（`requested_api_base`）
    /// 必须优先于 `config.api_base` —— 否则「TUI 里填的 URL」会被旧的
    /// `YUSHAN_API_BASE` 悄悄盖掉。
    #[test]
    fn test_login_custom_uses_requested_api_base_over_config() {
        let (mut config, dir) = test_config();
        config.api_base = Some("https://stale-from-env.example.com".into());
        let (state, state_dir) = test_state();
        let mut wiring = test_wiring();

        let out = login(
            &mut config,
            &mut wiring,
            &state,
            "custom",
            "sk-test",
            Some("https://typed-in-tui.example.com/v1"),
        );

        assert_eq!(
            config.api_base.as_deref(),
            Some("https://typed-in-tui.example.com/v1"),
            "用户显式填的 URL 优先于环境变量"
        );
        assert_eq!(wiring.model_id(), Some("dummy"), "应能据此构建 model");
        assert!(out.iter().any(|l| l.contains("✓ Logged in")), "{out:?}");

        // 落盘的也是用户填的那个 URL
        let mut reloaded = crate::provider::ProviderRegistry::new();
        reloaded.set_auth_override(dir.join("auth.json"));
        reloaded.load_auth();
        assert_eq!(
            reloaded.auth_for("custom").map(|e| e.api_base.as_str()),
            Some("https://typed-in-tui.example.com/v1")
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// 空白串的 `requested_api_base` 视同「未提供」—— 不得拿空 base 覆盖内置 base。
    #[test]
    fn test_login_blank_requested_api_base_falls_back_to_builtin() {
        let (mut config, dir) = test_config();
        let (state, state_dir) = test_state();
        let mut wiring = test_wiring();

        let out = login(
            &mut config,
            &mut wiring,
            &state,
            "deepseek",
            "sk-test",
            Some("   "),
        );

        assert_eq!(
            config.api_base.as_deref(),
            Some("https://api.deepseek.com"),
            "空白 URL 不得覆盖内置 base"
        );
        assert!(out.iter().any(|l| l.contains("✓ Logged in")), "{out:?}");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// `/model <name>`：config.model 更新 + 接线器换 model + state 落盘。
    #[test]
    fn test_set_model_switches_config_wiring_and_state() {
        let (mut config, dir) = test_config();
        config.api_base = Some("https://api.deepseek.com".into());
        config.api_key = Some("sk-x".into());
        config.provider = Some("deepseek".into());
        let (state, state_dir) = test_state();
        let mut wiring = test_wiring();

        let out = set_model(&mut config, &mut wiring, &state, "deepseek-reasoner", None);

        assert_eq!(config.model, "deepseek-reasoner");
        assert_eq!(wiring.model_id(), Some("dummy"), "应重建并交给接线器");
        assert_eq!(
            state.load().last_active_model.as_deref(),
            Some("deepseek-reasoner")
        );
        assert!(
            out.iter().any(|l| l.contains("Model switched to")),
            "{out:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// 缺凭证时 `/model`：config 记下名字，接线器**置空**（不留下会骗人的旧模型）。
    #[test]
    fn test_set_model_without_credentials_clears_wiring_model() {
        let (mut config, dir) = test_config();
        let (state, state_dir) = test_state();
        let mut wiring =
            Wiring::ephemeral(Some(Box::new(DummyModel)), Box::new(CollectingSink::new()));
        assert!(wiring.model_id().is_some());

        let out = set_model(&mut config, &mut wiring, &state, "gpt-4o", None);

        assert_eq!(config.model, "gpt-4o");
        assert!(
            wiring.model_id().is_none(),
            "构建失败 → 接线器不得留着旧模型"
        );
        assert!(
            out.iter().any(|l| l.contains("Cannot build model")),
            "{out:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// `/logout`：auth 落盘被移除、config 清空、接线器 model 置空、state 复位。
    #[test]
    fn test_logout_clears_everything() {
        let (mut config, dir) = test_config();
        config.api_base = Some("https://api.deepseek.com".into());
        config.api_key = Some("sk-x".into());
        config.provider = Some("deepseek".into());
        config
            .registry
            .save_auth(
                "deepseek",
                &AuthEntry {
                    api_base: "https://api.deepseek.com".into(),
                    api_key: "sk-x".into(),
                    model: "deepseek-chat".into(),
                },
            )
            .unwrap();
        let (state, state_dir) = test_state();
        state
            .save(&AppState {
                last_active_provider: Some("deepseek".into()),
                last_active_model: Some("deepseek-chat".into()),
            })
            .unwrap();
        let mut wiring =
            Wiring::ephemeral(Some(Box::new(DummyModel)), Box::new(CollectingSink::new()));

        let out = logout(&mut config, &mut wiring, &state);

        assert!(config.api_base.is_none());
        assert!(config.api_key.is_none());
        assert!(config.provider.is_none());
        assert!(wiring.model_id().is_none());
        assert!(out.iter().any(|l| l.contains("Logged out")), "{out:?}");

        let mut reloaded = crate::provider::ProviderRegistry::new();
        reloaded.set_auth_override(dir.join("auth.json"));
        reloaded.load_auth();
        assert!(reloaded.auth_for("deepseek").is_none(), "凭证应从磁盘移除");
        assert_eq!(state.load(), AppState::default(), "state 应复位");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// `/new`（持久会话）：换到新文件，旧文件保留。
    #[tokio::test]
    async fn test_new_session_switches_session_file() {
        use ys_session::{JsonlSession, Session as _};

        let dir = temp_dir("newsession");
        let mut wiring = Wiring::persistent_new(None, Box::new(CollectingSink::new()), dir.clone())
            .await
            .unwrap();
        let old = wiring.session_path().unwrap().to_path_buf();
        wiring
            .ports()
            .session
            .append(ys_core::Message {
                role: ys_core::Role::User,
                content: vec![ys_core::ContentBlock::Text { text: "hi".into() }],
            })
            .await
            .unwrap();

        let out = new_session(&mut wiring).await;

        let new = wiring.session_path().unwrap().to_path_buf();
        assert_ne!(old, new);
        assert!(old.exists(), "旧会话文件保留");
        // `/new` 惰性：只换句柄，未追加前不落盘新文件。
        assert!(!new.exists(), "/new 不预建空文件");
        assert!(wiring.session_messages().is_empty());
        assert!(out[0].contains("New conversation started"), "{out:?}");
        // 追加后新文件才落盘，且是可读的 jsonl。
        wiring
            .ports()
            .session
            .append(ys_core::Message {
                role: ys_core::Role::User,
                content: vec![ys_core::ContentBlock::Text { text: "new".into() }],
            })
            .await
            .unwrap();
        assert!(new.exists(), "追加后新会话文件落盘");
        let reopened = JsonlSession::open(&new).await.unwrap();
        assert_eq!(reopened.messages().len(), 1, "新文件只含新会话内容");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `/compact`（真压缩）：会话**变小且含摘要**，**而非变空**。
    ///
    /// 改前是占位 `clear_session()` —— 历史被静默丢弃、会话文件被截断。
    ///
    /// 窗口 3000 ⇒ 保留预算 300 token；6 条各 100 token ⇒ 尾扫留 3 条、摘掉 3 条
    /// ⇒ 结果 3 条保留 + 1 条摘要 = 4 条（少于 6 条）。
    ///
    /// 变异：退回 `wiring.clear_session()` → 会话为空 ⇒ 本用例第一条断言变红。
    #[tokio::test]
    async fn test_compact_summarizes_session_instead_of_clearing_it() {
        let dir = temp_dir("compact_summarize");
        let model = MockModel::new("mock");
        model.push_text("HANDOFF SUMMARY");
        let mut wiring = Wiring::persistent_new(
            Some(Box::new(model)),
            Box::new(CollectingSink::new()),
            dir.clone(),
        )
        .await
        .unwrap();
        seed(&mut wiring, 6, 100).await;
        let before = wiring.session_messages().to_vec();
        assert_eq!(before.len(), 6);

        let out = compact(&mut wiring, &ys_component::CompactPolicy::default(), 3000).await;

        let after = wiring.session_messages();
        assert!(
            !after.is_empty(),
            "压缩**不是**清空：会话必须仍有消息（退回 clear_session 即变红）"
        );
        assert!(
            after.len() < before.len(),
            "压缩后条数必须变少：{} → {}",
            before.len(),
            after.len()
        );
        assert!(
            ys_loop::token::estimate_session_tokens(after)
                < ys_loop::token::estimate_session_tokens(&before),
            "压缩后估算 token 必须更小"
        );
        assert!(
            after.iter().any(|m| text_of(m).contains("HANDOFF SUMMARY")),
            "会话里应含摘要文本：{after:?}"
        );
        assert!(
            after.iter().any(|m| text_of(m).starts_with("m5")),
            "尾部保留段必须逐字还在：{after:?}"
        );
        assert!(
            !after.iter().any(|m| text_of(m).starts_with("m0")),
            "被摘要掉的首条不应再逐字出现（摘要取代了它）"
        );

        // 汇报走返回值（`Outbound::Output`）—— 本模块绝不 print。
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].starts_with("Compacted:"), "{out:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **无 model 时 `/compact` 什么都不做**，只给一行明确提示。
    ///
    /// ⚠️ 这是本刀风险最高的点：若在 `model == None` 时退回 `clear_session()`，
    /// 「压不了」就变成「历史没了」—— 静默数据丢失。
    ///
    /// 变异：把无 model 分支改成 `wiring.clear_session()` → 会话被清空 ⇒ 变红。
    #[tokio::test]
    async fn test_compact_without_model_keeps_session_intact() {
        let dir = temp_dir("compact_no_model");
        let mut wiring = Wiring::persistent_new(None, Box::new(CollectingSink::new()), dir.clone())
            .await
            .unwrap();
        seed(&mut wiring, 6, 100).await;
        let before = wiring.session_messages().to_vec();

        let out = compact(&mut wiring, &ys_component::CompactPolicy::default(), 3000).await;

        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].contains("no model configured"), "{out:?}");
        assert!(
            out[0].contains("/login"),
            "提示必须可操作（指向 /login）：{out:?}"
        );
        assert_eq!(
            wiring.session_messages(),
            before.as_slice(),
            "无 model 时必须逐条不变 —— 绝不退回 clear_session"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **摘要失败 ⇒ session 一个字节都不动**（B-3 在 `/compact` 路径上的落地）：
    /// 报错、且内存消息与磁盘文件逐字节不变。
    ///
    /// 变异：失败臂改成 `clear_session()` → 会话为空、文件被截断 ⇒ 变红。
    #[tokio::test]
    async fn test_compact_failure_leaves_session_byte_identical() {
        let dir = temp_dir("compact_failure");
        let model = MockModel::new("mock");
        model.push_error("provider 挂了");
        let mut wiring = Wiring::persistent_new(
            Some(Box::new(model)),
            Box::new(CollectingSink::new()),
            dir.clone(),
        )
        .await
        .unwrap();
        seed(&mut wiring, 6, 100).await;
        let before = wiring.session_messages().to_vec();
        let path = wiring.session_path().unwrap().to_path_buf();
        let file_before = std::fs::read(&path).unwrap();

        let out = compact(&mut wiring, &ys_component::CompactPolicy::default(), 3000).await;

        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].contains("Compaction failed"), "{out:?}");
        assert!(out[0].contains("provider 挂了"), "原因应可读：{out:?}");
        assert_eq!(
            wiring.session_messages(),
            before.as_slice(),
            "失败时内存消息必须逐条不变"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            file_before,
            "失败时会话文件必须逐字节不变"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 消息太少（`len < 2`）→ `Nothing to compact.`，且**不调用 model**
    /// （`MockModel` 空队列被调用即 panic，正好把这条耦合钉成可执行断言）。
    #[tokio::test]
    async fn test_compact_with_too_few_messages_says_nothing_to_compact() {
        let dir = temp_dir("compact_too_few");
        let model = MockModel::new("mock"); // 队列为空：被调用即 panic
        let mut wiring = Wiring::persistent_new(
            Some(Box::new(model)),
            Box::new(CollectingSink::new()),
            dir.clone(),
        )
        .await
        .unwrap();
        seed(&mut wiring, 1, 100).await;
        let before = wiring.session_messages().to_vec();

        let out = compact(&mut wiring, &ys_component::CompactPolicy::default(), 3000).await;

        assert_eq!(out, vec!["Nothing to compact.".to_string()], "{out:?}");
        assert_eq!(wiring.session_messages(), before.as_slice());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `/export <path>`：会话文件被复制到目标路径。
    #[tokio::test]
    async fn test_export_copies_session_file_to_target() {
        let dir = temp_dir("export");
        let mut wiring = Wiring::persistent_new(None, Box::new(CollectingSink::new()), dir.clone())
            .await
            .unwrap();
        wiring
            .ports()
            .session
            .append(ys_core::Message {
                role: ys_core::Role::User,
                content: vec![ys_core::ContentBlock::Text {
                    text: "exported".into(),
                }],
            })
            .await
            .unwrap();

        let target = dir.join("out.jsonl");
        let out = export(&wiring, Some(target.clone()));

        assert!(out[0].contains("Exported"), "{out:?}");
        let copied = std::fs::read_to_string(&target).unwrap();
        assert!(copied.contains("exported"), "{copied}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 无 path → 落点即会话文件自身。**不得自拷**：`fs::copy` 对「源 == 目标」
    /// 会先截断目标 —— 那不是导出，是把会话清空。
    ///
    /// 变异：去掉 `is_same_file` 护栏 → 下面的 `contains("kept")` 变红（文件被清空）。
    #[tokio::test]
    async fn test_export_without_path_does_not_truncate_session_file() {
        let dir = temp_dir("export_default");
        let mut wiring = Wiring::persistent_new(None, Box::new(CollectingSink::new()), dir.clone())
            .await
            .unwrap();
        // `JsonlSession` 惰性落盘：先写一条，文件才真实存在。
        wiring
            .ports()
            .session
            .append(ys_core::Message {
                role: ys_core::Role::User,
                content: vec![ys_core::ContentBlock::Text {
                    text: "kept".into(),
                }],
            })
            .await
            .unwrap();
        let path = wiring.session_path().unwrap().to_path_buf();
        assert!(path.exists());

        let out = export(&wiring, None);

        assert!(out[0].contains(&path.display().to_string()), "{out:?}");
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("kept"),
            "自拷贝会把会话文件清空：{out:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 显式把目标指成会话文件自身时，同样走护栏（不截断）。
    #[tokio::test]
    async fn test_export_to_session_path_itself_is_guarded() {
        let dir = temp_dir("export_self");
        let mut wiring = Wiring::persistent_new(None, Box::new(CollectingSink::new()), dir.clone())
            .await
            .unwrap();
        wiring
            .ports()
            .session
            .append(ys_core::Message {
                role: ys_core::Role::User,
                content: vec![ys_core::ContentBlock::Text {
                    text: "kept".into(),
                }],
            })
            .await
            .unwrap();
        let path = wiring.session_path().unwrap().to_path_buf();

        let out = export(&wiring, Some(path.clone()));

        assert!(out[0].contains("already persisted"), "{out:?}");
        assert!(std::fs::read_to_string(&path).unwrap().contains("kept"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一次性会话没有会话文件 → 明确告知，不静默 no-op。
    #[test]
    fn test_export_ephemeral_session_says_nothing_to_export() {
        let wiring = test_wiring();
        let out = export(&wiring, None);
        assert!(out[0].contains("nothing to export"), "{out:?}");
    }

    /// **跨 provider 选模型**：`/model` 浮层的候选是**所有已登录 provider** 的并集，
    /// 选中别家的模型必须连端点一起切 —— 只改 `config.model` 会把 A 家的模型名发到
    /// B 家的 base（换来一个 400）。
    ///
    /// 变异：去掉 `set_model` 开头的 `provider_for_model` 分支 → 下面
    /// `provider` / `api_base` / `api_key` 三条断言同时变红。
    #[test]
    fn test_set_model_switches_provider_when_model_belongs_to_another() {
        let (mut config, dir) = test_config();
        config.api_base = Some("https://api.deepseek.com".into());
        config.api_key = Some("sk-ds".into());
        config.provider = Some("deepseek".into());
        config
            .registry
            .save_auth(
                "minimax",
                &AuthEntry {
                    api_base: "https://api.minimax.chat/v1".into(),
                    api_key: "sk-mm".into(),
                    model: "MiniMax-Text-01".into(),
                },
            )
            .unwrap();
        let (state, state_dir) = test_state();
        let mut wiring = test_wiring();

        let out = set_model(&mut config, &mut wiring, &state, "MiniMax-Text-01", None);

        assert_eq!(config.provider.as_deref(), Some("minimax"), "{out:?}");
        assert_eq!(
            config.api_base.as_deref(),
            Some("https://api.minimax.chat/v1"),
            "端点必须跟着切，否则模型名会发到别家 base"
        );
        assert_eq!(config.api_key.as_deref(), Some("sk-mm"));
        assert_eq!(config.model, "MiniMax-Text-01");
        assert!(
            out.iter()
                .any(|l| l.contains("Provider switched: deepseek → minimax")),
            "切换应告知用户：{out:?}"
        );
        assert_eq!(
            state.load().last_active_provider.as_deref(),
            Some("minimax"),
            "state 记的也应是新 provider"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// 归属查不到的模型名（手敲的自定义名）→ **保持当前 provider 不变**，
    /// 不把端点带跑。
    ///
    /// 变异：把 `provider_for_model` 的 `None` 臂也当成「切 provider」→ 变红。
    #[test]
    fn test_set_model_unknown_model_keeps_current_provider() {
        let (mut config, dir) = test_config();
        config.api_base = Some("https://api.deepseek.com".into());
        config.api_key = Some("sk-ds".into());
        config.provider = Some("deepseek".into());
        config
            .registry
            .save_auth(
                "minimax",
                &AuthEntry {
                    api_base: "https://api.minimax.chat/v1".into(),
                    api_key: "sk-mm".into(),
                    model: "MiniMax-Text-01".into(),
                },
            )
            .unwrap();
        let (state, state_dir) = test_state();
        let mut wiring = test_wiring();

        let out = set_model(&mut config, &mut wiring, &state, "my-local-model", None);

        assert_eq!(config.provider.as_deref(), Some("deepseek"), "{out:?}");
        assert_eq!(config.api_base.as_deref(), Some("https://api.deepseek.com"));
        assert!(
            !out.iter().any(|l| l.contains("Provider switched")),
            "未知名不该切 provider：{out:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// **显式归属压倒反查**：两个 provider 的目录里出现**逐字同名**的 id 时，
    /// `provider_for_model` 的答案是任意的（它遍历 `model_cache`，`HashMap` 迭代顺序
    /// 不确定）—— 靠它定位会把端点**静默切到错的一家**。浮层选中时带上来的 provider
    /// 必须直接采用。
    ///
    /// 变异：忽略 `provider` 形参、仍走 `provider_for_model` ⇒ 命中任一家（本用例里
    /// 期望的是 opencode-go，而反查先撞到谁不由本用例决定，但**必然**不再保证是它）
    /// ⇒ 端点/密钥断言变红。这正是「静默切错 base」的回归锚。
    #[test]
    fn test_set_model_prefers_explicit_provider_over_lookup() {
        let (mut config, dir) = test_config();
        config.provider = Some("deepseek".into());
        config.api_base = Some("https://api.deepseek.com".into());
        config.api_key = Some("sk-ds".into());
        for (name, base, key) in [
            ("deepseek", "https://api.deepseek.com", "sk-ds"),
            ("opencode-go", "https://opencode.ai/zen/go/v1", "sk-oc"),
        ] {
            config
                .registry
                .save_auth(
                    name,
                    &AuthEntry {
                        api_base: base.into(),
                        api_key: key.into(),
                        model: "shared-flash".into(),
                    },
                )
                .unwrap();
            config.registry.seed_cached_models(base, &["shared-flash"]);
        }
        let (state, state_dir) = test_state();
        let mut wiring = test_wiring();

        let out = set_model(
            &mut config,
            &mut wiring,
            &state,
            "shared-flash",
            Some("opencode-go"),
        );

        assert_eq!(config.provider.as_deref(), Some("opencode-go"), "{out:?}");
        assert_eq!(
            config.api_base.as_deref(),
            Some("https://opencode.ai/zen/go/v1"),
            "端点必须跟显式归属走，而不是反查挑中的那家"
        );
        assert_eq!(config.api_key.as_deref(), Some("sk-oc"));
        assert_eq!(config.model, "shared-flash");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// `/model deepseek/deepseek-chat` 直发：`/` 前是**已知 provider 名** ⇒ 拆开，
    /// `model` 落盘为**裸名**（前缀只用于定位端点）。
    #[test]
    fn test_set_model_qualified_name_splits_when_prefix_is_known_provider() {
        let (mut config, dir) = test_config();
        config.provider = Some("minimax".into());
        config.api_base = Some("https://api.minimax.chat/v1".into());
        config.api_key = Some("sk-mm".into());
        config
            .registry
            .save_auth(
                "deepseek",
                &AuthEntry {
                    api_base: "https://api.deepseek.com".into(),
                    api_key: "sk-ds".into(),
                    model: "deepseek-chat".into(),
                },
            )
            .unwrap();
        let (state, state_dir) = test_state();
        let mut wiring = test_wiring();

        let out = set_model(
            &mut config,
            &mut wiring,
            &state,
            "deepseek/deepseek-chat",
            None,
        );

        assert_eq!(config.provider.as_deref(), Some("deepseek"), "{out:?}");
        assert_eq!(config.model, "deepseek-chat", "model 必须是裸名，不带前缀");
        assert!(
            out.iter()
                .any(|l| l.contains("Provider switched: minimax → deepseek")),
            "切换应告知用户：{out:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// **网关的模型 id 可以自带 `/`**（`vendor/model` 形态）⇒ 前缀**不是**已知 provider
    /// 名时整串当模型 id —— 否则会被误切成「provider=vendor」，把端点带跑。
    ///
    /// 变异：`split_qualified_model` 去掉 `registry.find_provider(prefix).is_none()`
    /// 这一判据（任何 `/` 都拆）⇒ `config.model` 变 `claude-3`、provider 变
    /// `anthropic` ⇒ 本用例变红。
    #[test]
    fn test_set_model_slash_model_not_split_when_prefix_unknown() {
        let (mut config, dir) = test_config();
        config.provider = Some("deepseek".into());
        config.api_base = Some("https://api.deepseek.com".into());
        config.api_key = Some("sk-ds".into());
        let (state, state_dir) = test_state();
        let mut wiring = test_wiring();

        let out = set_model(&mut config, &mut wiring, &state, "anthropic/claude-3", None);

        assert_eq!(
            config.model, "anthropic/claude-3",
            "前缀非已知 provider ⇒ 整串是模型 id：{out:?}"
        );
        assert_eq!(
            config.provider.as_deref(),
            Some("deepseek"),
            "未知名不得改端点：{out:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// **N2 —— 顺序变异（I1）**：模型解析失败 ⇒ **会话 / 模型全不变**，只回错误文案。
    ///
    /// 当前实现把所有可失败步（解析目标 / 读 meta / 解析模型四字段 / 构建候选 model）
    /// **全部前置**到 `wiring.resume_into` 之前；`resume_into` 一旦执行就已换会话，
    /// 故模型解析失败时当前会话必须仍是旧的。
    ///
    /// **变异**：把 `resume_into` 挪到模型解析**之前**（或删掉候选构建的前置检查）
    /// ⇒ 会话已被替换 ⇒ 本条「必须是旧会话」断言变红。
    #[tokio::test]
    async fn resume_session_model_failure_leaves_session_untouched() {
        let dir = temp_dir("resume_model_fail");

        // 目标会话文件：**可被 open**（确保失败来自模型解析，而非 open）。
        let target_path = dir.join("target.jsonl");
        let mut meta = SessionMeta::new(0);
        meta.provider = Some("ghost".into()); // 无凭证的 provider
        std::fs::write(
            &target_path,
            format!("{}\n", serde_json::to_string(&meta).unwrap()),
        )
        .unwrap();

        let mut wiring = Wiring::persistent_new(None, Box::new(CollectingSink::new()), dir.clone())
            .await
            .unwrap();
        let old_path = wiring.session_path().unwrap().to_path_buf();
        let old_model = wiring.model_id().map(str::to_string);

        // 未配置的 cfg（无 env 凭证）+ 空 auth ⇒ `resolve_model_from_meta` 返回 None。
        //
        // env 锁**只圈住这次 `getenv`**（`Config::from_env`），不跨下方的 `.await`
        // —— `MutexGuard` 非 `Send`，跨 await 会让 future 失去 `Send` 并触发 clippy。
        let mut cfg = {
            let _guard = crate::test_env::env_lock();
            Config::from_env().unwrap()
        };
        cfg.api_base = None;
        cfg.api_key = None;
        cfg.provider = None;
        cfg.registry.set_auth_override(dir.join("auth.json"));
        let (state, state_dir) = test_state();

        let (lines, resumed) = resume_session(&mut cfg, &mut wiring, &state, "target").await;

        assert!(resumed.is_none(), "模型解析失败 ⇒ 未切换：{lines:?}");
        assert_eq!(
            wiring.session_path(),
            Some(old_path.as_path()),
            "模型解析失败时必须是**旧**会话（变异：resume_into 前置 ⇒ 本断言变红）"
        );
        assert_eq!(
            wiring.model_id().map(str::to_string),
            old_model,
            "模型解析失败时模型不得变"
        );
        assert_eq!(lines.len(), 1, "应恰回一条错误文案：{lines:?}");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }
}
