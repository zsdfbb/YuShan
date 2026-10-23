//! TUI 侧的命令表与解析（设计 §3 / §6）。
//!
//! **表在 TUI 侧**：命令的**用户可见行为**（提示什么、问什么）归 UI；
//! **能力实现**归 app 线程。`Request` 是二者之间唯一的契约 —— 漏实现是
//! **编译错误**（app 侧 `match Request` 必须穷尽），不会静默漂移。
//!
//! ```text
//! 输入 ──parse()──► Action ──┬─► Local(LocalAction)        TUI 自己处理（进 transcript）
//!                            ├─► Request(Request)          → app 线程（信道 ①）
//!                            ├─► Prompt(PromptKind)         → 模态浮层 → Request
//!                            ├─► Abort                      → 轮边界（信道 ②）
//!                            └─► Quit                       退出 UI
//! ```
//!
//! 本模块**纯逻辑、零终端**：`parse` 与三个文案构造函数都不碰 IO，因此可以
//! 直接单测（浮层与模态选择器在 `run.rs` / `draw.rs`）。

use std::path::PathBuf;

use ys_core::{ContentBlock, EffortLevel, Message, Role};
use ys_protocol::Request;

use crate::app::App;
use crate::transcript::TranscriptLine;
use crate::view::{CodingView, SkillEntry};

/// 模态选择器一次显示的选项条数（超出后在窗口内滚动）。
pub const PROMPT_PAGE_SIZE: usize = 8;

/// 命令表的一行。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    /// 命令名（不含前导 `/`），如 `"model"`。
    pub name: &'static str,
    /// 一句话说明（浮层与 `/help` 共用）。
    pub description: &'static str,
    /// 参数提示，如 `"[model_name]"`；无参数为 `None`。
    pub arg_hint: Option<&'static str>,
}

/// 内置命令表 —— **与设计 §6 的表逐条对应**，顺序即显示顺序。
///
/// 补全浮层（`completion.rs`）、`/help`（[`help_text`]）与解析（[`parse`]）三者
/// 都从这一张表派生，故「浮层里看得到、敲下去却不认识」在结构上不可能发生。
/// `/effort` 的用法串 —— 解析失败与无参显示**共用同一份文本**，
/// 避免两处措辞漂移。
const EFFORT_USAGE: &str = "用法：/effort [off|minimal|low|medium|high|xhigh|max|default]";

/// `/effort` 的「清除设置」令牌 —— 对应 `Option<EffortLevel>` 的 `None`。
const EFFORT_DEFAULT_TOKEN: &str = "default";

static COMMANDS: [CommandSpec; 14] = [
    CommandSpec {
        name: "help",
        description: "显示可用命令",
        arg_hint: Some("[command]"),
    },
    CommandSpec {
        name: "status",
        description: "显示当前配置与状态",
        arg_hint: None,
    },
    CommandSpec {
        name: "skills",
        description: "查看并选择一个 skill",
        arg_hint: None,
    },
    CommandSpec {
        name: "copy",
        description: "复制最后一条回复（v1：写进对话区，手动选中）",
        arg_hint: None,
    },
    CommandSpec {
        name: "quit",
        description: "退出",
        arg_hint: None,
    },
    CommandSpec {
        name: "thinking",
        description: "开关思考内容的显示",
        arg_hint: Some("[on|off]"),
    },
    CommandSpec {
        name: "effort",
        description: "设置模型思考档位",
        arg_hint: Some("[off|minimal|low|medium|high|xhigh|max|default]"),
    },
    CommandSpec {
        name: "model",
        description: "切换模型（无参时浮层选择）",
        arg_hint: Some("[model_name]"),
    },
    CommandSpec {
        name: "login",
        description: "登录 provider 并保存凭证",
        arg_hint: None,
    },
    CommandSpec {
        name: "logout",
        description: "登出并清除凭证",
        arg_hint: None,
    },
    CommandSpec {
        name: "new",
        description: "开新会话",
        arg_hint: None,
    },
    CommandSpec {
        name: "resume",
        description: "恢复既有会话（无参列出并选择）",
        arg_hint: Some("[id]"),
    },
    CommandSpec {
        name: "compact",
        description: "压缩上下文",
        arg_hint: None,
    },
    CommandSpec {
        name: "export",
        description: "导出会话到文件",
        arg_hint: Some("[path]"),
    },
];

/// 内置命令表（顺序即显示顺序）。
pub fn all_commands() -> &'static [CommandSpec] {
    &COMMANDS
}

/// 一次输入的解析结果。
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// 什么都不做（空输入、光秃秃的 `/`）。
    None,
    /// 发给 app 线程的请求（信道 ①）。
    Request(Request),
    /// 中途取消（信道 ② —— 轮边界）。
    ///
    /// 当前 `parse()` 不产出 —— Ctrl+C/Esc 在 `handle_key` 里直接投
    /// `Boundary::Abort`；保留以备将来 `/abort`。`events::dispatch` 的处理
    /// 路径由 `test_abort_action_still_routes_to_boundary` 钉住。
    Abort,
    /// 退出 UI。
    Quit,
    /// 本地动作（TUI 自己处理，不需要 app）。
    Local(LocalAction),
    /// 需要模态浮层收集输入，拿到结果后再发 `Request`。
    Prompt(PromptKind),
}

/// 完全在 TUI 侧完成、只需往 transcript 里写一条的行文动作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalAction {
    /// 打印命令表；`Some(name)` 时只打印该命令的详情（`/help model`）。
    Help(Option<String>),
    /// 打印当前状态（`provider` / `model` / tokens / 轮数 / 会话文件 / 工具 / cwd）。
    Status,
    /// 把最后一条 assistant 回复写进对话区（v1 无剪贴板依赖）。
    Copy,
    /// `Some(on)` 显式设置，`None` 为 toggle。
    Thinking(Option<bool>),
    /// 无参 `/effort`：显示当前档位与用法（**不发请求**；真值在 `CodingView.effort`）。
    Effort,
    /// 本地提示（未知命令 / 参数错误）—— 文本原样进 transcript。
    Hint(String),
}

/// 需要模态浮层收集输入的命令。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    /// 选 provider → 问 api_key →（该 provider 无内置 base 时）问 API base URL
    /// → `Request::Login`。
    Login,
    /// 选模型 → `Request::SetModel`。
    PickModel,
    /// 选 skill → **回填输入框**（`/<command> `），**不发 `Request`**。
    ///
    /// 与 [`PickModel`](Self::PickModel) 的区别是有意的：skill 常常带参数
    /// （`/git-status --short`），选中只把命令名填进输入框，用户补完参数再回车；
    /// 且 `-p` / `--json` 无 TUI，不涉及此路。落地在
    /// [`crate::prompter::PromptOutcome::Prefill`]。
    PickSkill,
    /// 选会话 → `Request::ResumeSession { target }`。
    ///
    /// **不来自 [`parse`]**：`/resume` 无参产出的是 `Request::ListSessions`（一次往返），
    /// 由 app 侧的 `Outbound::SessionList` 经 `App::apply_outbound` 设上本值 ——
    /// 候选数据（`App.pending_sessions`）只有那时才在 UI 手上。解析逻辑见
    /// [`crate::prompter::resolve_session_pick`]（候选与 `&CodingView` **不同源**，
    /// 故不走 `resolve_prompt`）。
    PickSession,
}

/// 解析一次提交的输入。
///
/// - 非 `/` 开头 → [`Action::Request`]`(`[`Request::Prompt`]`)`（普通消息）
/// - 未知 `/xxx` → 本地提示（[`Action::Local`]`(`[`LocalAction::Hint`]`)`）
/// - 见 [`all_commands`] 的逐条映射
///
/// **不认识 skill** —— 等价于 [`parse_with_skills`]`(input, &[])`。需要认 skill
/// 名的调用点（TUI 的 `submit`）走 [`parse_with_skills`]。
pub fn parse(input: &str) -> Action {
    parse_with_skills(input, &[])
}

/// 同 [`parse`]，但额外认识 skill 名（`/git-status <args>`）。
///
/// **硬序：内置命令 > skill** —— 先走内置表，**未命中**才查 skill。判定只写在兜底
/// 分支里，故「装一条名为 `status` 的 skill 就能让 `/status` 失效」在结构上不可能。
///
/// 命中 skill ⇒ [`Action::Request`]`(`[`Request::Prompt`]`(原文))` —— **原文、零展开**：
/// TUI 不读盘、不碰 frontmatter，把 SKILL.md 正文塞进上下文是 app 侧
/// （`skills::expand_invocation`）的事（设计 §关键数据流 路线 2）。
pub fn parse_with_skills(input: &str, skills: &[SkillEntry]) -> Action {
    let text = input.trim();
    if text.is_empty() {
        return Action::None;
    }
    if !text.starts_with('/') {
        return Action::Request(Request::Prompt(prompt_message(text)));
    }

    let mut parts = text[1..].split_whitespace();
    let name = parts.next().unwrap_or("");
    let args: Vec<&str> = parts.collect();
    let arg = || args.join(" ");

    match name {
        "" => Action::None, // 光秃秃一个 `/`
        // 无参 = 全表；有参 = 只显示该命令（`arg_hint` 里的 `[command]` 不是空承诺）
        "help" => Action::Local(LocalAction::Help(if args.is_empty() {
            None
        } else {
            Some(arg())
        })),
        "status" => Action::Local(LocalAction::Status),
        // 开选择器列全部 skill（带 description）。**选中不直发** —— 回填输入框，
        // 用户补参数后自行回车（见 `PromptKind::PickSkill`）。
        "skills" => Action::Prompt(PromptKind::PickSkill),
        "copy" => Action::Local(LocalAction::Copy),
        "quit" => Action::Quit,
        "thinking" => match args.first().copied() {
            None => Action::Local(LocalAction::Thinking(None)),
            Some("on") => Action::Local(LocalAction::Thinking(Some(true))),
            Some("off") => Action::Local(LocalAction::Thinking(Some(false))),
            Some(other) => Action::Local(LocalAction::Hint(format!(
                "用法：/thinking [on|off]（`{other}` 不是有效取值）"
            ))),
        },
        // 无参 = 本地显示当前档位（真值在视图里，TUI 不持有第二份）；
        // 有参 = 直发 `SetEffort`。`default` 是**清除**令牌 —— 发 `None`，
        // 不是第八个枚举值（`EffortLevel::from_str` 刻意拒绝它）。
        //
        // 大小写不敏感（与 `EffortLevel::from_str` 一致）；非法值只本地提示，
        // **不改变任何状态**（视图由 app 侧快照推送，本地没有可改的真值）。
        "effort" => {
            if args.is_empty() {
                Action::Local(LocalAction::Effort)
            } else {
                let value = arg();
                if value.eq_ignore_ascii_case(EFFORT_DEFAULT_TOKEN) {
                    Action::Request(Request::SetEffort { effort: None })
                } else {
                    match value.parse::<EffortLevel>() {
                        Ok(level) => Action::Request(Request::SetEffort {
                            effort: Some(level),
                        }),
                        Err(_) => Action::Local(LocalAction::Hint(EFFORT_USAGE.to_string())),
                    }
                }
            }
        }
        // **无参不直发 SetModel** —— 先开浮层选（设计 §6）。
        //
        // 有参时**原样透传**（`provider: None`）：限定名 `provider/model` 的拆分
        // **不在这里做** —— 本层拿不到 provider 名单（`parse` 是纯函数，签名里没有
        // registry），见 `capabilities::set_model`。UI 只负责「发用户敲的那串」。
        "model" => {
            if args.is_empty() {
                Action::Prompt(PromptKind::PickModel)
            } else {
                Action::Request(Request::SetModel {
                    model: arg(),
                    provider: None,
                })
            }
        }
        "login" => Action::Prompt(PromptKind::Login),
        "logout" => Action::Request(Request::Logout),
        "new" => Action::Request(Request::NewSession),
        // 无参 → 先拉列表（一次往返：`ListSessions` → `SessionList` → 浮层）；
        // 有参 → 直接把 id/路径交给 app 侧的 `sessions::resolve`（与 `/model` 双形态对称）。
        "resume" => {
            if args.is_empty() {
                Action::Request(Request::ListSessions)
            } else {
                Action::Request(Request::ResumeSession { target: arg() })
            }
        }
        "compact" => Action::Request(Request::Compact),
        // 无参 → `None`：**默认落点由 app 侧按会话文件决定**（设计 §6「会话文件
        // 在 app 侧」）。TUI 不替 app 猜一个相对 cwd 的路径（那样会静默覆盖同名文件）。
        "export" => Action::Request(Request::Export {
            path: if args.is_empty() {
                None
            } else {
                Some(PathBuf::from(arg()))
            },
        }),
        _ => skill_fallback(text, name, skills),
    }
}

/// 内置表**未命中**时的兜底：首 token 命中一条 skill ⇒ 原文直发；否则既有未知命令提示。
///
/// `command` 是**目录名**（= `/` 后要敲的那个词），与内置命令名同处一个命名空间。
/// 精确相等才命中（`/git-status` 不会因为存在 skill `git` 而误命中）。
///
/// **不判 `user_invocable`**：`CodingView.skills` 是**已过滤的快照** —— 准入
/// （`user-invocable: false` 的条目只给模型、不给用户敲）发生在 **app 侧
/// `view::build_view`**，不可被用户调用的 skill 根本不会进这份快照。TUI 侧只
/// 消费已过滤的结果，`SkillEntry` 也刻意不携带 `user_invocable`（数据/显示分离，
/// UI 不做准入决策）。
fn skill_fallback(text: &str, name: &str, skills: &[SkillEntry]) -> Action {
    if skills.iter().any(|s| s.command == name) {
        return Action::Request(Request::Prompt(prompt_message(text)));
    }
    Action::Local(LocalAction::Hint(unknown_hint(text)))
}

/// 普通输入 → 一条用户消息。
pub fn prompt_message(text: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::Text { text: text.into() }],
    }
}

/// 未知命令的本地提示（含可用命令表）。
pub fn unknown_hint(input: &str) -> String {
    format!("未知命令：{input}\n{}", help_text())
}

/// `/help` 的正文 —— 与补全浮层同源（同一张 [`all_commands`]）。**不含 skill**：
/// 薄包装 [`help_text_with_skills`]`(&[])`。
pub fn help_text() -> String {
    let mut out = String::from("可用命令：\n");
    for spec in all_commands() {
        out.push_str("  ");
        out.push_str(&spec_line(spec));
        out.push('\n');
    }
    out.push('\n');
    out.push_str(KEY_HELP);
    out.trim_end().to_string()
}

/// `/help` 里的**键位**段 —— 导航与输入编辑。
///
/// **为什么要有这一段**：滚动键（`↑↓` / `PageUp` / `End` …）此前没有任何界面出口，
/// 用户只能试 —— 而「跳到最新消息」是高频操作，试不出来就等于没有。
///
/// 键位取 **Mac / Linux 通用**的那一套：`Ctrl+↑/↓` 与 `Ctrl+Home/End` 都是
/// 终端无关的（`CSI 1;5A` 一类）。**不列 `⌘` 组合** —— macOS 终端自己消费 `⌘`，
/// 字节到不了应用（见 `events.rs` 模块头）。
const KEY_HELP: &str = "键位：
  ↑ ↓                输入框为空时 ↑ 取回历史（↓ 还原草稿）；否则滚动对话
  PageUp / PageDown  翻页（一页 = 视窗高度）
  滚轮 / 触控板上下   逐行滚动对话（不影响输入历史）
  End                跳到最新消息
  Ctrl+↓ / Ctrl+End  跳到最新消息
  Ctrl+↑ / Ctrl+Home 跳到最早消息
  Home               输入行首（Ctrl+A 同义）
  Ctrl+A / Ctrl+E    输入行首 / 行尾
  ← →                输入光标（Ctrl 或 ⌥ + ← → 按词移动）
  Enter              提交（Shift/Alt+Enter 或 Ctrl+J 换行）
  Tab                补全浮层
  Esc                取消回合 / 清空输入
  Ctrl+C             取消回合；再按一次退出
  Ctrl+D             退出";

/// `/help` 的正文（**含 skill 段**）—— 内置命令表之后追加一段 skill 名清单。
///
/// **只列 `/<command>`，不列 description**（也不列 `label`）：真实语料的 description
/// 是 CJK 密集的长句（实测 56 条合计 ~18 KB），列进 `/help` 会把命令索引变成正文
/// 倾倒。补全浮层（[`crate::completion`]）是**另一个面** —— 那里列 description 是
/// 为了帮用户挑，而这里只需要「有哪些」。
pub fn help_text_with_skills(skills: &[SkillEntry]) -> String {
    let mut out = help_text();
    if skills.is_empty() {
        return out;
    }
    let mut commands: Vec<&str> = skills.iter().map(|s| s.command.as_str()).collect();
    commands.sort_unstable();
    commands.dedup();

    out.push_str("\n\n可用 skill：\n");
    for command in commands {
        out.push_str("  /");
        out.push_str(command);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// `/help <command>` 的正文：**只**渲染该命令那一行（`name / arg_hint / description`）。
///
/// 未知命令名 → 本地提示 + 完整命令表（与 [`unknown_hint`] 同构），
/// 而不是「安静地打一整张表装作什么都没发生」。
pub fn command_help_text(name: &str) -> String {
    match all_commands().iter().find(|spec| spec.name == name) {
        Some(spec) => spec_line(spec),
        None => format!("未知命令：/{name}\n{}", help_text()),
    }
}

/// 命令表里一行的渲染：`/model [model_name]  — 切换模型`。
///
/// 补全浮层与 `/help` **共用**本函数 —— 两处措辞不会漂移。
pub fn spec_line(spec: &CommandSpec) -> String {
    let mut line = format!("/{}", spec.name);
    if let Some(hint) = spec.arg_hint {
        line.push(' ');
        line.push_str(hint);
    }
    line.push_str("  — ");
    line.push_str(spec.description);
    line
}

/// `/status` 的正文（数据全部来自 [`CodingView`]）。
pub fn status_text(view: &CodingView) -> String {
    let dash = || "-".to_string();
    let tokens = format!(
        "↑{} ↓{}",
        crate::format::format_tokens(view.total_input_tokens),
        crate::format::format_tokens(view.total_output_tokens),
    );
    let logged_in = if view.logged_in_providers.is_empty() {
        dash()
    } else {
        view.logged_in_providers.join(", ")
    };
    let session = view
        .session_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(dash);
    let tools = if view.tools.is_empty() {
        dash()
    } else {
        view.tools.join(", ")
    };
    // skill 条数；被整表字节预算收起的条数一并显示（R3：模型看不到的那部分，
    // 用户要在 `/status` 看得见）。
    let skills = if view.skills.is_empty() {
        dash()
    } else if view.skills_omitted == 0 {
        view.skills.len().to_string()
    } else {
        format!("{} ({} omitted)", view.skills.len(), view.skills_omitted)
    };
    let ctx = view
        .context_window
        .map(|c| c.to_string())
        .unwrap_or_else(dash);
    let effort = view
        .effort
        .map(|level| level.to_string())
        .unwrap_or_else(|| EFFORT_DEFAULT_TOKEN.to_string());

    [
        format!("provider   {}", view.provider.clone().unwrap_or_else(dash)),
        format!("model      {}", view.model_label()),
        format!("effort: {effort}"),
        format!("tokens     {tokens}"),
        format!("turns      {}", view.turn_count),
        format!("messages   {}", view.message_count),
        // **两个时长口径**：`duration` = 存活（进程起止），`executed` = 执行（Σ 回合实际占用）。
        // 二者之差是「用户不在时 TUI 空等输入」的时长 —— **不是缺陷，是事实** ⇒ 分开报。
        format!("duration   {}", view.session_duration_str()),
        format!("executed   {}", view.executed_duration_str()),
        format!("logged in  {logged_in}"),
        format!(
            "known      {}",
            if view.providers.is_empty() {
                dash()
            } else {
                view.provider_names().join(", ")
            }
        ),
        format!("context    {ctx}"),
        format!("session    {session}"),
        format!("tools      {tools}"),
        format!("skills     {skills}"),
        format!("cwd        {}", view.cwd.display()),
    ]
    .join("\n")
}

/// `/copy` 的正文：最后一条非空 assistant 文本。
///
/// **v1 无剪贴板依赖**（不为一个命令引一个跨平台剪贴板 crate）：内容写进
/// transcript，用终端自身的选中复制即可。
pub fn copy_text(app: &App) -> String {
    let last = app.transcript.iter().rev().find_map(|line| match line {
        TranscriptLine::Assistant(s) if !s.trim().is_empty() => Some(s.as_str()),
        _ => None,
    });
    match last {
        Some(s) => format!("（/copy v1 占位：无剪贴板集成，内容如下，可手动选中）\n{s}"),
        None => "（/copy：还没有可复制的回复）".to_string(),
    }
}

/// 执行一个本地动作：写一条 `System` 行进 transcript 并重新贴底。
pub fn apply_local(app: &mut App, action: LocalAction) {
    let text = match action {
        LocalAction::Help(target) => match target {
            // 全表用**带 skill 段**的版本：skill 是 `/` 命名空间的一部分，
            // `/` 浮层里看得见、`/help` 里也必须看得见。
            None => help_text_with_skills(&app.view.skills),
            // `/help <name>` 只认内置命令（skill 的详情在它的 SKILL.md 里，
            // `/help` 不替它做正文倾倒 —— 见 [`help_text_with_skills`]）。
            Some(name) => command_help_text(&name),
        },
        LocalAction::Status => status_text(&app.view),
        LocalAction::Copy => copy_text(app),
        LocalAction::Thinking(explicit) => {
            app.thinking_visible = explicit.unwrap_or(!app.thinking_visible);
            format!(
                "thinking: {}",
                if app.thinking_visible { "on" } else { "off" }
            )
        }
        // **只读视图**：档位的真值归 app 侧（`/effort <level>` 发请求后由下一份
        // 快照推回来），这里不写 `app.view.effort` —— 否则 UI 会先于 app 显示
        // 一个尚未落盘的状态。
        LocalAction::Effort => format!(
            "effort: {}（{EFFORT_USAGE}）",
            app.view
                .effort
                .map(|level| level.to_string())
                .unwrap_or_else(|| EFFORT_DEFAULT_TOKEN.to_string())
        ),
        LocalAction::Hint(text) => text,
    };
    app.transcript.push(TranscriptLine::System(text));
    app.follow = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use ys_core::ToolCallId;
    use ys_protocol::Request;

    fn app() -> App {
        App::new(CodingView::for_test())
    }

    /// 设计 §6 的表 —— 逐条穷举。**这是本模块的主断言**：
    /// 任何一条命令的映射被改动，这里必须同步改（否则红）。
    #[test]
    fn test_every_command_maps_to_the_designed_action() {
        use LocalAction as L;
        let cases: &[(&str, Action)] = &[
            ("/help", Action::Local(L::Help(None))),
            ("/help model", Action::Local(L::Help(Some("model".into())))),
            (
                "/help   model  ",
                Action::Local(L::Help(Some("model".into()))),
            ),
            ("/status", Action::Local(L::Status)),
            ("/skills", Action::Prompt(PromptKind::PickSkill)),
            ("/copy", Action::Local(L::Copy)),
            ("/quit", Action::Quit),
            ("/thinking", Action::Local(L::Thinking(None))),
            ("/thinking on", Action::Local(L::Thinking(Some(true)))),
            ("/thinking off", Action::Local(L::Thinking(Some(false)))),
            ("/effort", Action::Local(L::Effort)),
            (
                "/effort high",
                Action::Request(Request::SetEffort {
                    effort: Some(EffortLevel::High),
                }),
            ),
            (
                "/effort default",
                Action::Request(Request::SetEffort { effort: None }),
            ),
            (
                "/effort maybe",
                Action::Local(L::Hint(EFFORT_USAGE.to_string())),
            ),
            ("/model", Action::Prompt(PromptKind::PickModel)),
            (
                "/model gpt-4o",
                Action::Request(Request::SetModel {
                    model: "gpt-4o".into(),
                    provider: None,
                }),
            ),
            ("/login", Action::Prompt(PromptKind::Login)),
            ("/logout", Action::Request(Request::Logout)),
            ("/new", Action::Request(Request::NewSession)),
            ("/resume", Action::Request(Request::ListSessions)),
            (
                "/resume abc",
                Action::Request(Request::ResumeSession {
                    target: "abc".into(),
                }),
            ),
            ("/compact", Action::Request(Request::Compact)),
            ("/export", Action::Request(Request::Export { path: None })),
            (
                "/export a.jsonl",
                Action::Request(Request::Export {
                    path: Some(PathBuf::from("a.jsonl")),
                }),
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(&parse(input), expected, "输入 `{input}`");
        }
    }

    /// 命令表**自身**的完整性：表里每一条 `Request` 都真的能被产出来。
    ///
    /// 两条来源都算：`parse` 直发的，以及模态浮层经 `resolve_prompt` 拿到的
    /// （`/login` → `Login`、`/model` 无参 → `SetModel`，这里用 `FakePrompter`
    /// 实跑一遍，而不是「假设它应该产出什么」）。
    ///
    /// **别指望本测试拦住「`Request` 新增变体但忘了加命令」** —— 它只逐个
    /// `matches!` 已知变体，新增变体不会让它变红。真正与 `Request`
    /// 对齐的闭环在 app 侧：app 循环对 `Request` 的穷尽 `match` 是**编译错误**，
    /// 漏实现编不过。本测试只保证这张表本身完整（每条命令都真能产出请求，
    /// 而不是解析成 `None` / `Hint` 被悄悄吞掉）。
    #[test]
    fn test_command_table_covers_every_request_variant() {
        let mut produced: Vec<Request> = [
            "hello",
            "/model gpt",
            "/effort high",
            "/logout",
            "/new",
            "/resume",
            "/resume abc",
            "/compact",
            "/export p",
        ]
        .iter()
        .filter_map(|i| match parse(i) {
            Action::Request(r) => Some(r),
            _ => None,
        })
        .collect();

        let view = CodingView::for_test();
        // `/model` 浮层的答案必须是**显示串**（`Prompter::select` 的契约：回传候选里的一项）。
        // 从视图派生而非写死 —— 本用例的题目是「每条命令都能产出请求」，
        // 不该与标签格式耦合（格式另有 `ModelEntry::label` 的专门用例）。
        let model_choice = crate::view::ModelEntry::label(&view.available_models[0]);
        let fake = crate::prompter::FakePrompter::with_answers(vec![
            Ok("deepseek".to_string()),
            Ok("sk-1".to_string()),
            Ok(model_choice),
        ]);
        for input in ["/login", "/model"] {
            match parse(input) {
                Action::Prompt(kind) => produced.push(
                    match crate::prompter::resolve_prompt(kind, &fake, &view)
                        .unwrap_or_else(|e| panic!("`{input}` 应能产出 Request：{e}"))
                    {
                        crate::prompter::PromptOutcome::Request(r) => r,
                        // 回填型命令（`/skills`）不在这份输入清单里；真加进来就说明
                        // 这条断言该改成「回填」而非「产出 Request」。
                        other => panic!("`{input}` 应产出 Request，得到 {other:?}"),
                    },
                ),
                other => panic!("`{input}` 应走模态浮层，得到 {other:?}"),
            }
        }

        let has = |pred: fn(&Request) -> bool| produced.iter().any(pred);
        assert!(
            has(|r| matches!(r, Request::Prompt(_))),
            "非 / 输入必须产出 Prompt"
        );
        assert!(
            has(|r| matches!(r, Request::SetModel { .. })),
            "缺 SetModel"
        );
        assert!(
            has(|r| matches!(r, Request::SetEffort { effort: Some(_) })),
            "缺 SetEffort（设定档位）"
        );
        assert!(has(|r| matches!(r, Request::Login { .. })), "缺 Login");
        assert!(has(|r| matches!(r, Request::Logout)), "缺 Logout");
        assert!(has(|r| matches!(r, Request::NewSession)), "缺 NewSession");
        assert!(
            has(|r| matches!(r, Request::ListSessions)),
            "缺 ListSessions"
        );
        assert!(
            has(|r| matches!(r, Request::ResumeSession { .. })),
            "缺 ResumeSession"
        );
        assert!(has(|r| matches!(r, Request::Compact)), "缺 Compact");
        assert!(has(|r| matches!(r, Request::Export { .. })), "缺 Export");
    }

    #[test]
    fn test_plain_input_becomes_prompt_request() {
        match parse("重构这个模块") {
            Action::Request(Request::Prompt(msg)) => {
                assert_eq!(msg.role, Role::User);
                assert!(
                    matches!(&msg.content[0], ContentBlock::Text { text } if text == "重构这个模块")
                );
            }
            other => panic!("普通输入应产出 Prompt，得到 {other:?}"),
        }
    }

    #[test]
    fn test_unknown_command_hints_locally() {
        let action = parse("/nope");
        match action {
            Action::Local(LocalAction::Hint(text)) => {
                assert!(text.contains("未知命令：/nope"), "{text}");
                // 必须列出可用命令
                for spec in all_commands() {
                    assert!(text.contains(spec.name), "提示里缺 /{}：{text}", spec.name);
                }
            }
            other => panic!("未知命令应本地提示，得到 {other:?}"),
        }
    }

    #[test]
    fn test_blank_and_bare_slash_are_none() {
        assert_eq!(parse(""), Action::None);
        assert_eq!(parse("   "), Action::None);
        assert_eq!(parse("/"), Action::None);
    }

    #[test]
    fn test_thinking_bad_argument_hints_usage() {
        match parse("/thinking maybe") {
            Action::Local(LocalAction::Hint(t)) => assert!(t.contains("on|off"), "{t}"),
            other => panic!("非法参数应本地提示，得到 {other:?}"),
        }
    }

    /// `/effort` 的四种形态：无参本地显示、合法档位直发、`default` 清除、
    /// 非法值本地提示。
    ///
    /// 变异：删掉 `default` 分支 ⇒ `/effort default` 落进 `parse::<EffortLevel>`
    /// 的 `Err` 分支变成 `Hint` ⇒ 本测试与
    /// [`test_every_command_maps_to_the_designed_action`] 同时变红。
    #[test]
    fn test_effort_parse_forms() {
        assert_eq!(parse("/effort"), Action::Local(LocalAction::Effort));
        assert_eq!(
            parse("/effort HIGH"),
            Action::Request(Request::SetEffort {
                effort: Some(EffortLevel::High),
            }),
            "大小写不敏感由 EffortLevel::from_str 决定"
        );
        assert_eq!(
            parse("/effort xhigh"),
            Action::Request(Request::SetEffort {
                effort: Some(EffortLevel::XHigh),
            })
        );
        assert_eq!(
            parse("/effort default"),
            Action::Request(Request::SetEffort { effort: None }),
            "default = 清除设置，不是第八个枚举值"
        );
        // `default` 令牌同样大小写不敏感（`eq_ignore_ascii_case` 分支）。
        // 变异：把该比较改成 `==` ⇒ 这两条变红（落进 `parse::<EffortLevel>` 的 `Err`）。
        for input in ["/effort DEFAULT", "/effort Default"] {
            assert_eq!(
                parse(input),
                Action::Request(Request::SetEffort { effort: None }),
                "`{input}` 应识别为清除令牌"
            );
        }
        // 前后与中间的多余空白由 `trim` + `split_whitespace` 吞掉。
        assert_eq!(
            parse("/effort   HIGH "),
            Action::Request(Request::SetEffort {
                effort: Some(EffortLevel::High),
            }),
            "多余空白不影响档位解析"
        );
        match parse("/effort maybe") {
            Action::Local(LocalAction::Hint(text)) => {
                assert!(text.contains("用法：/effort"), "{text}");
                for legal in [
                    "off", "minimal", "low", "medium", "high", "xhigh", "max", "default",
                ] {
                    assert!(text.contains(legal), "用法缺 `{legal}`：{text}");
                }
            }
            other => panic!("非法档位应本地提示，得到 {other:?}"),
        }
    }

    /// 无参 `/effort` 只读视图：恰好一条 System 行，档位 + 用法同处一行。
    ///
    /// 变异：把 `None` 的显示改回 `-` 或空串 ⇒ 本测试**不受影响**（它在 apply 前
    /// 显式设了 `Some(High)`，不经过 `None` 分支），只有
    /// [`test_apply_local_effort_shows_default_when_unset`] 变红。
    #[test]
    fn test_apply_local_effort_shows_current_level_and_usage() {
        let mut a = app();
        a.view.effort = Some(EffortLevel::High);
        apply_local(&mut a, LocalAction::Effort);

        assert_eq!(a.transcript.len(), 1, "只写一条：{:?}", a.transcript);
        let Some(TranscriptLine::System(s)) = a.transcript.last() else {
            panic!("应 push 一条 System：{:?}", a.transcript);
        };
        assert_eq!(
            s,
            "effort: high（用法：/effort [off|minimal|low|medium|high|xhigh|max|default]）"
        );
        assert!(a.follow, "本地输出后重新贴底");
    }

    /// `view.effort == None` ⇒ 显示 `default`（provider 默认）。
    #[test]
    fn test_apply_local_effort_shows_default_when_unset() {
        let mut a = app();
        assert!(a.view.effort.is_none());
        apply_local(&mut a, LocalAction::Effort);

        assert_eq!(a.transcript.len(), 1, "只写一条：{:?}", a.transcript);
        let Some(TranscriptLine::System(s)) = a.transcript.last() else {
            panic!("应 push 一条 System：{:?}", a.transcript);
        };
        assert!(
            s.starts_with("effort: default（用法：/effort "),
            "None 显示 default：{s}"
        );
    }

    /// `/status` 的档位行：`None` → `default`，`Some` → 规范拼写。
    #[test]
    fn test_status_text_reports_effort() {
        let line = |v: &CodingView| {
            status_text(v)
                .lines()
                .find(|l| l.starts_with("effort:"))
                .expect("`/status` 应有 effort 行")
                .to_string()
        };

        let mut view = CodingView::for_test();
        assert_eq!(line(&view), "effort: default");

        view.effort = Some(EffortLevel::High);
        assert_eq!(line(&view), "effort: high");

        view.effort = Some(EffortLevel::XHigh);
        assert_eq!(line(&view), "effort: xhigh", "用规范拼写而非 Debug 名");
    }

    /// A1 的用户可见面：`/status` 必须**同时**报两个时长口径。
    ///
    /// 只有一个「duration」行 ⇒ 用户会把「存活 15 小时」误读成「干了 15 小时」；
    /// 实测确有会话执行 3.22 h / 存活 15.10 h 的形态 ⇒ 两个口径必须分开。
    ///
    /// 变异：删掉 `executed` 行 ⇒ 变红。
    #[test]
    fn status_text_reports_both_duration_notions() {
        let mut view = CodingView::for_test();
        view.executed = std::time::Duration::from_secs(3 * 3600 + 12 * 60);

        let text = status_text(&view);
        let duration = text
            .lines()
            .find(|l| l.starts_with("duration "))
            .expect("`/status` 应有 duration 行");
        let executed = text
            .lines()
            .find(|l| l.starts_with("executed "))
            .expect("`/status` 应有 executed 行（执行口径）");

        assert_eq!(duration, "duration   0s", "存活行仍由 session_started 派生");
        assert_eq!(executed, "executed   3h12m", "执行行显示 Σ 回合实际占用");
        assert_ne!(duration, executed, "两个口径必须分行显示，不能合并成一行");
    }

    /// 空白与大小写：`/model` 无参**必须**走浮层，绝不直发 SetModel。
    ///
    /// 变异：把无参分支改成 `Request::SetModel{..}` → 本测试与
    /// [`test_every_command_maps_to_the_designed_action`] 同时变红。
    #[test]
    fn test_bare_model_never_sends_set_model_directly() {
        assert_eq!(parse("/model"), Action::Prompt(PromptKind::PickModel));
        assert_eq!(parse("/model  "), Action::Prompt(PromptKind::PickModel));
    }

    #[test]
    fn test_command_table_shape() {
        let names: Vec<&str> = all_commands().iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            vec![
                "help", "status", "skills", "copy", "quit", "thinking", "effort", "model", "login",
                "logout", "new", "resume", "compact", "export",
            ],
            "命令表与设计 §6 逐条对应（顺序即显示顺序）"
        );
        for spec in all_commands() {
            assert!(!spec.description.is_empty(), "{} 缺说明", spec.name);
        }
        assert_eq!(
            all_commands()
                .iter()
                .find(|c| c.name == "model")
                .unwrap()
                .arg_hint,
            Some("[model_name]"),
            "浮层要显示 arg_hint（设计 §2.4）"
        );
        assert_eq!(
            all_commands()
                .iter()
                .find(|c| c.name == "effort")
                .unwrap()
                .arg_hint,
            Some("[off|minimal|low|medium|high|xhigh|max|default]"),
            "合法取值必须在浮层可见"
        );
    }

    /// `/resume` 的三处同源：`all_commands()`（补全浮层的来源）、`/help` 正文、`parse`。
    ///
    /// 变异：把 `COMMANDS` 里的 `resume` 条目删掉（或把 `parse` 的 `resume` 分支去掉）
    /// ⇒ 本测试的对应断言变红。
    #[test]
    fn test_resume_command_is_registered_and_parsed() {
        let spec = all_commands()
            .iter()
            .find(|c| c.name == "resume")
            .expect("命令表里必须有 resume");
        assert_eq!(spec.arg_hint, Some("[id]"));
        assert!(!spec.description.is_empty());

        assert!(
            help_text().contains("/resume [id]"),
            "`/help` 必须列出 /resume 及其参数提示：{}",
            help_text()
        );

        assert_eq!(parse("/resume"), Action::Request(Request::ListSessions));
        assert_eq!(
            parse("/resume 2026-10-10-abc"),
            Action::Request(Request::ResumeSession {
                target: "2026-10-10-abc".into()
            })
        );
    }

    #[test]
    fn test_spec_line_renders_hint_and_description() {
        let spec = CommandSpec {
            name: "model",
            description: "切换模型",
            arg_hint: Some("[model_name]"),
        };
        assert_eq!(spec_line(&spec), "/model [model_name]  — 切换模型");
        let bare = CommandSpec {
            name: "quit",
            description: "退出",
            arg_hint: None,
        };
        assert_eq!(spec_line(&bare), "/quit  — 退出");
    }

    #[test]
    fn test_help_text_lists_every_command() {
        let text = help_text();
        for spec in all_commands() {
            assert!(
                text.contains(&format!("/{}", spec.name)),
                "缺 {}：{text}",
                spec.name
            );
        }
        assert!(text.starts_with("可用命令："), "{text}");
    }

    /// `/help <command>` **只**给这一条 —— 其余命令名一律不得出现，否则等于又打
    /// 了一遍全表，「针对性帮助」就是空话（`arg_hint: Some("[command]")` 是承诺）。
    ///
    /// 变异：把 `parse` 的 `"help"` 分支改回无条件 `Help(None)`（全表）→ 本测试变红。
    #[test]
    fn test_help_with_command_shows_only_that_command() {
        assert_eq!(
            parse("/help model"),
            Action::Local(LocalAction::Help(Some("model".into())))
        );

        let mut a = app();
        apply_local(&mut a, LocalAction::Help(Some("model".into())));
        let Some(TranscriptLine::System(text)) = a.transcript.last() else {
            panic!("应 push 一条 System：{:?}", a.transcript);
        };
        assert!(text.contains("/model [model_name]"), "{text}");
        assert!(text.contains("切换模型"), "{text}");
        for spec in all_commands().iter().filter(|s| s.name != "model") {
            assert!(
                !text.contains(&format!("/{}", spec.name)),
                "`/help model` 不该出现 /{}：{text}",
                spec.name
            );
        }
    }

    /// `/help <未知命令>` → 明说未知 + 附可用命令表（不静默装无事发生）。
    ///
    /// 变异：把未知分支的 `help_text()` 去掉 → 本测试变红。
    #[test]
    fn test_help_with_unknown_command_hints_and_lists_commands() {
        let mut a = app();
        apply_local(&mut a, LocalAction::Help(Some("nope".into())));
        let Some(TranscriptLine::System(text)) = a.transcript.last() else {
            panic!("应 push 一条 System：{:?}", a.transcript);
        };
        assert!(text.contains("未知命令：/nope"), "{text}");
        for spec in all_commands() {
            assert!(
                text.contains(&format!("/{}", spec.name)),
                "提示里缺 /{}：{text}",
                spec.name
            );
        }
    }

    #[test]
    fn test_apply_local_help_pushes_system_line() {
        let mut a = app();
        apply_local(&mut a, LocalAction::Help(None));
        assert!(
            matches!(a.transcript.last(), Some(TranscriptLine::System(s)) if s.contains("/model"))
        );
        assert!(a.follow, "本地输出后重新贴底");
    }

    #[test]
    fn test_apply_local_status_reports_view_fields() {
        let mut a = app();
        a.view.total_input_tokens = 1_200;
        a.view.total_output_tokens = 345;
        a.view.turn_count = 2;
        apply_local(&mut a, LocalAction::Status);
        let Some(TranscriptLine::System(s)) = a.transcript.last() else {
            panic!("应 push 一条 System：{:?}", a.transcript);
        };
        for needle in [
            "provider",
            "deepseek",
            "deepseek-chat",
            "↑1.2k ↓345",
            "turns      2",
            "effort: default",
            "tools",
            "read, write",
            "/tmp",
        ] {
            assert!(s.contains(needle), "status 缺 `{needle}`：{s}");
        }
    }

    #[test]
    fn test_apply_local_thinking_toggles_and_sets() {
        let mut a = app();
        assert!(!a.thinking_visible);

        apply_local(&mut a, LocalAction::Thinking(None));
        assert!(a.thinking_visible, "无参 = toggle");
        apply_local(&mut a, LocalAction::Thinking(None));
        assert!(!a.thinking_visible, "再 toggle 回来");

        apply_local(&mut a, LocalAction::Thinking(Some(true)));
        assert!(a.thinking_visible, "显式 on 幂等");
        apply_local(&mut a, LocalAction::Thinking(Some(true)));
        assert!(a.thinking_visible);
        apply_local(&mut a, LocalAction::Thinking(Some(false)));
        assert!(!a.thinking_visible, "显式 off");
    }

    /// `/help` 必须列出**键位** —— 滚动键此前没有任何界面出口（用户只能试）。
    #[test]
    fn test_help_text_contains_key_bindings() {
        let text = help_text();
        assert!(text.contains("键位："), "help 必须有键位段");
        for key in [
            "End",
            "Ctrl+↓",
            "Ctrl+↑",
            "Ctrl+Home",
            "Ctrl+End",
            "PageUp",
            "PageDown",
            "滚轮",
            "Home",
        ] {
            assert!(text.contains(key), "键位段必须列出 {key}");
        }
        // `⌘` 组合到不了应用（终端自己消费）⇒ 不得出现在键位表里。
        assert!(!text.contains('⌘'), "键位表不得列 ⌘ 组合（终端消费不到）");
    }

    #[test]
    fn test_copy_takes_last_assistant_text() {
        let mut a = app();
        a.transcript.push(TranscriptLine::User("q".into()));
        a.transcript.push(TranscriptLine::Assistant("first".into()));
        a.transcript
            .push(TranscriptLine::Assistant("second".into()));
        apply_local(&mut a, LocalAction::Copy);
        let text = copy_text(&a);
        // push 之后 last 是刚才那条 System
        let Some(TranscriptLine::System(s)) = a.transcript.last() else {
            panic!("应 push System");
        };
        assert!(s.contains("second"), "取最后一条：{s}");
        assert!(!s.contains("first"), "不取更早的：{s}");
        assert!(text.contains("second"));
    }

    /// `/copy` 的**占位**语义必须写在正文里（v1 无剪贴板，不是静默 no-op）。
    #[test]
    fn test_copy_without_assistant_reply_says_so() {
        let a = app();
        let text = copy_text(&a);
        assert!(text.contains("还没有可复制的回复"), "{text}");
    }

    #[test]
    fn test_copy_ignores_empty_and_non_assistant_lines() {
        let mut a = app();
        a.transcript.push(TranscriptLine::Assistant("   ".into()));
        a.transcript.push(TranscriptLine::Tool {
            id: ToolCallId("c".into()),
            name: "read".into(),
            summary: "x".into(),
            result: None,
            success: true,
        });
        a.transcript.push(TranscriptLine::Error("boom".into()));
        assert!(copy_text(&a).contains("还没有可复制的回复"));
    }

    #[test]
    fn test_apply_local_hint_pushes_text_verbatim() {
        let mut a = app();
        apply_local(&mut a, LocalAction::Hint("自定义提示".into()));
        assert!(
            matches!(a.transcript.last(), Some(TranscriptLine::System(s)) if s == "自定义提示")
        );
    }

    // -----------------------------------------------------------------------
    // skill 通路（`/skill-name`）—— TUI 只「认名」，零展开
    // -----------------------------------------------------------------------

    fn skill(command: &str, description: &str) -> SkillEntry {
        SkillEntry {
            command: command.into(),
            label: command.into(),
            description: description.into(),
        }
    }

    /// **内置命令 > skill** 是硬序：存在同名 skill 也不得改变内置命令的去向。
    ///
    /// 变异：把 skill 判定提到内置 `match` **之前** ⇒ 本测试变红。
    #[test]
    fn test_builtin_command_wins_over_same_named_skill() {
        let skills = [
            skill("status", "一条想遮蔽 /status 的 skill"),
            skill("model", "一条想遮蔽 /model 的 skill"),
            skill("quit", "一条想遮蔽 /quit 的 skill"),
        ];
        assert_eq!(
            parse_with_skills("/status", &skills),
            Action::Local(LocalAction::Status),
            "同名 skill 不得遮蔽内置命令"
        );
        assert_eq!(
            parse_with_skills("/model", &skills),
            Action::Prompt(PromptKind::PickModel)
        );
        assert_eq!(parse_with_skills("/quit", &skills), Action::Quit);
    }

    /// 命中 skill ⇒ **原文（含参数）**直发 app，TUI 零展开。
    ///
    /// 变异：把 `text.to_string()` 换成只发 `/{name}`（丢掉参数）⇒ 本测试变红。
    #[test]
    fn test_skill_hit_forwards_original_text_with_args() {
        let skills = [skill("git-status", "看仓库状态")];
        assert_eq!(
            parse_with_skills("/git-status 看看提交", &skills),
            Action::Request(Request::Prompt(prompt_message("/git-status 看看提交"))),
            "原文（含参数）原样发给 app"
        );
        assert_eq!(
            parse_with_skills("  /git-status   ", &skills),
            Action::Request(Request::Prompt(prompt_message("/git-status"))),
            "首尾空白按既有规则 trim，其余原样"
        );
    }

    /// 命中判据是**首 token 精确相等**：存在 skill `git` 不代表 `/git-status` 可用。
    ///
    /// 变异：把 `s.command == name` 改成 `name.starts_with(&s.command)` ⇒ 本测试变红。
    #[test]
    fn test_skill_match_is_exact_not_prefix() {
        let skills = [skill("git", "前缀不算命中")];
        assert!(
            matches!(
                parse_with_skills("/git-status", &skills),
                Action::Local(LocalAction::Hint(_))
            ),
            "前缀相同的另一条命令不得被认成 skill"
        );
        assert_eq!(
            parse_with_skills("/git log", &skills),
            Action::Request(Request::Prompt(prompt_message("/git log")))
        );
    }

    /// 未命中任何 skill ⇒ 仍是既有未知命令提示（回归）。
    #[test]
    fn test_unknown_command_still_hints_when_skills_present() {
        let skills = [skill("git-status", "看仓库状态")];
        match parse_with_skills("/zzz", &skills) {
            Action::Local(LocalAction::Hint(text)) => {
                assert!(text.contains("未知命令：/zzz"), "{text}");
                for spec in all_commands() {
                    assert!(
                        text.contains(&format!("/{}", spec.name)),
                        "提示里缺 /{}：{text}",
                        spec.name
                    );
                }
            }
            other => panic!("未命中应本地提示，得到 {other:?}"),
        }
    }

    /// 薄包装的正确性：`parse` ≡ `parse_with_skills(.., &[])`。
    #[test]
    fn test_parse_is_thin_wrapper_over_parse_with_skills() {
        for input in [
            "",
            "   ",
            "/",
            "hello",
            "/status",
            "/help model",
            "/thinking maybe",
            "/effort",
            "/effort high",
            "/effort default",
            "/effort maybe",
            "/model",
            "/model gpt-4o",
            "/export a.jsonl",
            "/nope",
            "/git-status x",
        ] {
            assert_eq!(
                parse(input),
                parse_with_skills(input, &[]),
                "输入 `{input}` 的两条路径必须逐字相同"
            );
        }
    }

    /// `/help` 列出 skill 名，**只列 `/<command>`**（不列 description、不列 label）。
    #[test]
    fn test_help_text_lists_skill_names_without_descriptions() {
        let skills = [
            SkillEntry {
                command: "git-status".into(),
                label: "Git Status".into(),
                description: "一条很长很长的描述，绝不该出现在 /help 里".into(),
            },
            skill("arch-design", "另一条描述"),
        ];
        let text = help_text_with_skills(&skills);
        assert!(text.contains("可用 skill："), "{text}");
        assert!(text.contains("/git-status"), "{text}");
        assert!(text.contains("/arch-design"), "{text}");
        assert!(
            !text.contains("一条很长很长的描述"),
            "不得倾倒 description：{text}"
        );
        assert!(!text.contains("另一条描述"), "不得倾倒 description：{text}");
        assert!(!text.contains("Git Status"), "label 也不列：{text}");

        // 内置命令表仍然完整，且整段排在 skill 段**之前**
        for spec in all_commands() {
            assert!(
                text.contains(&format!("/{}", spec.name)),
                "缺 /{}：{text}",
                spec.name
            );
        }
        assert!(
            text.find("/export").expect("内置表在")
                < text.find("/arch-design").expect("skill 段在"),
            "内置命令必须在前：{text}"
        );
        // 空切片 = 既有正文（薄包装）
        assert_eq!(help_text_with_skills(&[]), help_text());
    }

    /// `/help` 的**生产路径**（`apply_local` → `Help(None)`）也带上 skill 段 ——
    /// 否则 `/` 浮层里看得见、`/help` 里看不见（两处显示面不一致）。
    #[test]
    fn test_apply_local_help_lists_skills_from_view() {
        let mut a = app(); // `for_test` 视图带 commit / review 两条 skill
        apply_local(&mut a, LocalAction::Help(None));
        let Some(TranscriptLine::System(text)) = a.transcript.last() else {
            panic!("应 push 一条 System：{:?}", a.transcript);
        };
        assert!(text.contains("/commit"), "{text}");
        assert!(text.contains("/review"), "{text}");
        assert!(
            !text.contains("生成一条提交信息"),
            "不列 description：{text}"
        );
        assert!(text.contains("/model"), "内置表照旧：{text}");
    }

    /// `/status` 显示 skill 条数，以及被整表预算收起的条数（R3）。
    #[test]
    fn test_status_text_reports_skill_count_and_omitted() {
        let line = |v: &CodingView| {
            status_text(v)
                .lines()
                .find(|l| l.starts_with("skills"))
                .expect("`/status` 应有 skills 行")
                .to_string()
        };

        let mut view = CodingView::for_test();
        view.skills = vec![skill("a", "d"), skill("b", "d")];
        view.skills_omitted = 3;
        assert_eq!(line(&view), "skills     2 (3 omitted)");

        view.skills_omitted = 0;
        assert_eq!(line(&view), "skills     2", "无收起时只报条数");

        view.skills.clear();
        assert_eq!(line(&view), "skills     -", "无 skill 与其余空字段同形");
    }
}
