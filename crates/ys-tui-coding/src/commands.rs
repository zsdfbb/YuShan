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
use crate::theme::ThemeKind;
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

static COMMANDS: [CommandSpec; 17] = [
    CommandSpec {
        name: "help",
        description: "显示可用命令",
        arg_hint: Some("[command]"),
    },
    CommandSpec {
        name: "keybind",
        description: "列出全部快捷键",
        arg_hint: None,
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
        description: "复制最后一条回复的 Markdown 原文到系统剪贴板",
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
        name: "theme",
        description: "切换配色主题",
        arg_hint: Some("[name]"),
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
        name: "clear",
        description: "清空对话上下文并清屏（会话文件保留，可 /resume 找回）",
        arg_hint: None,
    },
    CommandSpec {
        name: "resume",
        description: "恢复既有会话（无参列出并选择）",
        arg_hint: Some("[id]"),
    },
    CommandSpec {
        name: "compact",
        // 文案不提「回合中则在下一轮边界执行」：那是投递路径的实现细节（ADR-0052），
        // 用户在 `/help` 里要的是「这条命令干什么、会看到什么」。
        description: "压缩上下文；先提示再报告前后估算",
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
    /// 打印**完整**键位表（分段；真源是 [`all_keybindings`]）。
    ///
    /// 与 `/help` 的分工：`/help` 是命令索引 + 一段高频键位摘要（完整表塞进去会把
    /// 要查的命令挤出屏幕），`/keybind` 是那份表的正本。
    Keybind,
    /// 把最后一条 assistant 回复的 **Markdown 原文**送入剪贴板通道（[`App::pending_clipboard`]，
    /// 由 `run.rs` 发 OSC 52）。
    Copy,
    /// `Some(on)` 显式设置，`None` 为 toggle。
    Thinking(Option<bool>),
    /// 无参 `/effort`：显示当前档位与用法（**不发请求**；真值在 `CodingView.effort`）。
    Effort,
    /// `/theme <name>`：**已解析**的主题。
    ///
    /// 携带 [`ThemeKind`] 而不是原始 `String`：非法输入必须在进入本地应用**之前**
    /// 就被拦下（解析在 `parse_with_skills` 里做，落成 [`Self::Hint`]），故本变体
    /// 的载荷天然是合法值，`apply_local` 不必再承担「解析失败怎么办」这一态。
    /// 无参 `/theme` 不走这里 —— 它开浮层（[`PromptKind::PickTheme`]）。
    Theme(ThemeKind),
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
    /// 选主题 → `Request::SetTheme { theme }`。
    ///
    /// **不走 [`crate::prompter::resolve_prompt`]**：主题浮层要**实时预览**
    /// （每移动一格就把候选色板铺到整屏），而 `Prompter::select` 是 `&self` 且无
    /// 回调，结构上拿不到「逐键」这件事。故由 `run.rs` 的专用模态
    /// （`apply_theme_modal`）处理，`resolve_prompt` 对本值一律返回 `Err`。
    PickTheme,
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
        "keybind" => Action::Local(LocalAction::Keybind),
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
        // 无参 → 浮层（要实时预览，见 [`PromptKind::PickTheme`]）；有参 → 本地立即
        // 生效（`apply_local` 调 `app.set_theme`），再由 `events.rs` 单向同步给 app
        // 持久化。非法值只本地提示**不改任何状态**（`ThemeParseError` 的回显已含
        // 全部合法名，故不再另造一份用法串）。
        "theme" => {
            if args.is_empty() {
                Action::Prompt(PromptKind::PickTheme)
            } else {
                match arg().parse::<ThemeKind>() {
                    Ok(kind) => Action::Local(LocalAction::Theme(kind)),
                    Err(e) => Action::Local(LocalAction::Hint(e.to_string())),
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
        // 与 `/new` 兄弟：都是换新会话文件（旧文件一个字节不动），差别只在 UI 侧
        // —— `clear` 成功后 app 发 `Outbound::SessionCleared`，UI 见它即清屏。
        "clear" => Action::Request(Request::ClearSession),
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
    out.push_str(KEY_HELP_BRIEF);
    out.trim_end().to_string()
}

/// 一个键位条目 —— `/keybind` 与 `/help` 的键位段**共用这一份**。
///
/// 结构化而不是一段字符串：两个出口（`/keybind` 详表、`/help` 摘要）需要的是同一批
/// 事实的**不同排布**，写两遍必然漂移（上一轮就出现过 `/help` 列 `End` 为「跳到最新
/// 消息」而实现已改成输入行尾的偏差）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyBinding {
    /// 键的组合，如 `"Ctrl+Shift+A"`；多个写法用 `" / "` 分隔。
    pub keys: &'static str,
    /// 一句话行为说明。
    pub action: &'static str,
    /// 分组（决定 `/keybind` 里的分段与小标题）。
    pub group: KeyGroup,
    /// `true` = 已经**不在**当前实现里，列出只为解释历史习惯（`/keybind` 里标 `（未绑定）`）。
    ///
    /// 目前无条目使用：它存在的理由是**别把「终端做不到」写成「不支持」**——
    /// 列在这里的键是「本仓刻意不绑」的（如 `⌘` 组合），需要一句原因。
    pub note: Option<&'static str>,
}

/// 键位分组（`/keybind` 的段序即枚举序）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyGroup {
    /// 输入编辑：光标、选区、删除。
    Editing,
    /// 提交、换行、补全、取消、退出。
    Submit,
    /// 聊天滚动与跳转。
    Scroll,
    /// 剪贴板。
    Clipboard,
    /// 鼠标。
    Mouse,
}

impl KeyGroup {
    /// 全部组（段序）。
    pub const ALL: [KeyGroup; 5] = [
        Self::Editing,
        Self::Submit,
        Self::Scroll,
        Self::Clipboard,
        Self::Mouse,
    ];

    /// 小标题（`/keybind` 的段头）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Editing => "输入编辑",
            Self::Submit => "提交与退出",
            Self::Scroll => "滚动与跳转",
            Self::Clipboard => "剪贴板",
            Self::Mouse => "鼠标",
        }
    }
}

/// 键位表 —— **`/keybind` 与 `/help` 的唯一真源**。
///
/// 条目顺序即 `/keybind` 的显示顺序（组内）。`/help` 只取其中高频的一段（见
/// [`KEY_HELP_BRIEF`]），避免把 `/help` 撑成第二份完整手册。
///
/// **不列 `⌘` 组合**：macOS 终端自己消费 `⌘`，字节到不了应用（见 `events.rs`
/// 模块头）。**不列 `⌥+↓`**：Terminal.app 发 `ESC[B`，与普通 `↓` 同形。
static KEYBINDINGS: [KeyBinding; 21] = [
    KeyBinding {
        keys: "← / →",
        action: "移动输入光标（Ctrl 或 ⌥ + ←/→ 按词移动；⌥b / ⌥f 同义）",
        group: KeyGroup::Editing,
        note: None,
    },
    KeyBinding {
        keys: "↑ / ↓",
        action: "输入框内按视觉行上下移动；到首/末行才回落历史 / 聊天滚动",
        group: KeyGroup::Editing,
        note: None,
    },
    KeyBinding {
        keys: "Home / End",
        action: "输入当前行的行首 / 行尾（多行时只到本行）",
        group: KeyGroup::Editing,
        note: None,
    },
    KeyBinding {
        keys: "Ctrl+A",
        action: "全选输入框（随后 Backspace 一下清空）",
        group: KeyGroup::Editing,
        note: Some("行首改走 Home / Ctrl+E 给行尾；部分终端把 Ctrl+Shift+A 压成它，两者同义"),
    },
    KeyBinding {
        keys: "Ctrl+E",
        action: "输入当前行行尾（与 End 同义）",
        group: KeyGroup::Editing,
        note: None,
    },
    KeyBinding {
        keys: "Ctrl+V",
        action: "粘贴系统剪贴板到输入框（有选区则替换整段）",
        group: KeyGroup::Editing,
        note: Some("读不到剪贴板（无本机工具 / SSH 远端）时无反应，改用终端的 Ctrl+Shift+V"),
    },
    KeyBinding {
        keys: "Shift + ← → ↑ ↓",
        action: "扩选（Home / End 同理；端点重合即收起）",
        group: KeyGroup::Editing,
        note: None,
    },
    KeyBinding {
        keys: "Backspace / Delete",
        action: "删除；有选区时删除整段选中",
        group: KeyGroup::Editing,
        note: None,
    },
    KeyBinding {
        keys: "Enter",
        action: "提交输入",
        group: KeyGroup::Submit,
        note: None,
    },
    KeyBinding {
        keys: "Shift+Enter / Alt+Enter / Ctrl+J",
        action: "输入框内换行（Ctrl+J 用于不支持键盘增强协议的终端）",
        group: KeyGroup::Submit,
        note: None,
    },
    KeyBinding {
        keys: "Tab",
        action: "补全浮层（输入以 `/` 开头时）；浮层内 Tab/Enter 填入、↑↓ 选择",
        group: KeyGroup::Submit,
        note: None,
    },
    KeyBinding {
        keys: "Esc",
        action: "回合中取消回合；idle 时清空输入",
        group: KeyGroup::Submit,
        note: None,
    },
    KeyBinding {
        keys: "Ctrl+C",
        action: "有选区时复制；否则取消回合（再按一次退出）",
        group: KeyGroup::Submit,
        note: None,
    },
    KeyBinding {
        keys: "Ctrl+D",
        action: "退出",
        group: KeyGroup::Submit,
        note: None,
    },
    KeyBinding {
        keys: "PageUp / PageDown",
        action: "翻页（一页 = 视窗高度 − 1 行）",
        group: KeyGroup::Scroll,
        note: None,
    },
    KeyBinding {
        keys: "Ctrl+↑ / Ctrl+Home",
        action: "跳到最早消息",
        group: KeyGroup::Scroll,
        note: None,
    },
    KeyBinding {
        keys: "Ctrl+↓ / Ctrl+End",
        action: "跳到最新消息",
        group: KeyGroup::Scroll,
        note: None,
    },
    KeyBinding {
        keys: "滚轮 / 触控板",
        action: "逐 1 视觉行滚动对话（不影响输入历史）",
        group: KeyGroup::Scroll,
        note: None,
    },
    KeyBinding {
        keys: "Ctrl+C",
        action: "复制输入框选区",
        group: KeyGroup::Clipboard,
        note: None,
    },
    KeyBinding {
        keys: "左键拖选（Chat 区）",
        action: "选中对话内容，松手即复制；⇧+拖选 = 用终端原生选择",
        group: KeyGroup::Mouse,
        note: None,
    },
    KeyBinding {
        keys: "/copy",
        action: "把最后一条回复的 Markdown 原文写入系统剪贴板",
        group: KeyGroup::Clipboard,
        note: None,
    },
];

/// 全部键位（顺序即 `/keybind` 的显示顺序）。
pub fn all_keybindings() -> &'static [KeyBinding] {
    &KEYBINDINGS
}

/// `/keybind` 的正文：按 [`KeyGroup::ALL`] 分段，逐条列键与行为（含注意事项）。
///
/// **为什么不只是 `/help` 的那一段**：`/help` 是命令索引，塞二十条键位会把真正要
/// 找的命令挤到屏幕外；而键位是「试不出来就等于没有」的那类信息，需要一个能整屏
/// 读完、且**与实现同源**的出口。
pub fn keybind_text() -> String {
    let mut out = String::from("键位：\n");
    for group in KeyGroup::ALL {
        let items: Vec<&KeyBinding> = all_keybindings()
            .iter()
            .filter(|b| b.group == group)
            .collect();
        if items.is_empty() {
            continue;
        }
        out.push('\n');
        out.push_str(group.label());
        out.push('\n');
        // 键列按**本组最长键**对齐：不对齐时中英混排的 `—` 会跳来跳去，扫读成本高。
        // 宽度用**显示列**（CJK 占 2 列）而不是 char 数 —— 否则 `滚轮 / 触控板` 这类
        // 条目会多缩两格（`draw::display_width` 是同一口径，纯计算、不碰终端）。
        let pad = items
            .iter()
            .map(|b| crate::draw::display_width(b.keys))
            .max()
            .unwrap_or(0);
        for b in items {
            out.push_str("  ");
            out.push_str(b.keys);
            for _ in crate::draw::display_width(b.keys)..pad {
                out.push(' ');
            }
            out.push_str("  — ");
            out.push_str(b.action);
            out.push('\n');
            // 注意事项**单独续行**：塞在行为说明后面会变成两层括号
            // （「…清空）（部分终端…）」），读起来像两个都不重要。
            if let Some(note) = b.note {
                out.push_str("  ");
                for _ in 2..pad + 4 {
                    out.push(' ');
                }
                out.push_str("· ");
                out.push_str(note);
                out.push('\n');
            }
        }
    }
    out.push_str(
        "\n终端限制：不绑 ⌘ 组合（终端自己消费）；不绑 ⌥+↓（Terminal.app 发 ESC[B，与 ↓ 同形）。",
    );
    out.trim_end().to_string()
}

/// `/help` 里那段**高频摘要**（完整表走 `/keybind`）。
///
/// 取的是「不知道就卡住」的那几条：滚动的三个入口、输入的行首行尾、提交/换行、
/// 全选与复制、取消与退出。完整键位与终端限制见 [`keybind_text`]。
const KEY_HELP_BRIEF: &str = "键位（完整表：/keybind）：
  ↑ ↓                输入框内先按视觉行移光标；到首/末行才取历史 / 滚动对话
  PageUp / PageDown  翻页；Ctrl+↑ / Ctrl+Home 最早，Ctrl+↓ / Ctrl+End 最新
  滚轮 / 触控板上下   逐行滚动对话（不影响输入历史）
  Home / End         输入行首 / 行尾（Ctrl+E 也是行尾）
  Ctrl+A             全选输入框（随后 Backspace 清空）
  Ctrl+V             粘贴系统剪贴板（读不到时用终端的 Ctrl+Shift+V）
  Shift + ←→↑↓       扩选（端点重合即收起）
  Enter              提交（Shift/Alt+Enter 或 Ctrl+J 换行）
  Ctrl+C             有选区时复制；否则取消回合，再按一次退出
  Ctrl+D             退出；Esc 取消回合 / 清空输入
  Tab                补全浮层";

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

/// context window 的缩写：`200k` / `1M` / `262k`。
///
/// 与 [`crate::format::format_tokens`] 的区别：窗口是**容量上限**，整千/整百万时
/// 不带小数（`200k` 而非 `200.0k`）；非整千向下取整到 k（`262_144` ⇒ `262k`），
/// 非整百万保留一位小数（`1_048_576` ⇒ `1.0M`）。取整而非四舍五入保证边界稳定
/// —— `999_999` 不会进位成 `1000k`。
fn format_context_window(n: usize) -> String {
    if n >= 1_000_000 {
        if n.is_multiple_of(1_000_000) {
            format!("{}M", n / 1_000_000)
        } else {
            // 截断而非四舍五入（与 k 档同规则）：`1_999_999` ⇒ `1.9M`，不进位成 `2.0M`。
            format!("{}.{}M", n / 1_000_000, (n / 100_000) % 10)
        }
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

/// `/status` 的正文（数据全部来自 [`CodingView`]）。
pub fn status_text(view: &CodingView) -> String {
    let dash = || "-".to_string();
    // token 行：**无读数不省略**，两侧写 `-`（省略等于把「没测到」藏起来）。
    let tokens = format!(
        "↑{} ↓{}",
        crate::format::format_tokens_or_dash(view.total_input_tokens),
        crate::format::format_tokens_or_dash(view.total_output_tokens),
    );
    // 缓存行：三态分开报 —— 无读数写明「provider 未上报」，
    // 有读数但算不出百分比（全零分母）给 `-`，其余给整数百分比。
    let cache = match view.cache_tokens {
        None => "- (provider 未上报)".to_string(),
        Some((hit, miss)) => match view.cache_hit_pct {
            Some(pct) => format!("{hit} / {miss}（{pct}%）"),
            None => format!("{hit} / {miss}（-）"),
        },
    };
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
    // context 行：只有 declared 与 effective **都存在且不等**时才并列显示
    // （effective 在前，declared 在后）并标 `(capped)`；相等或 declared 缺失
    // 保持单值原样 —— 这是既有快照的逐字节契约。
    let ctx = match (view.context_window, view.context_window_declared) {
        (Some(effective), Some(declared)) if effective != declared => format!(
            "{} / {} (capped)",
            format_context_window(effective),
            format_context_window(declared)
        ),
        (Some(effective), _) => effective.to_string(),
        (None, _) => dash(),
    };
    let effort = view
        .effort
        .map(|level| level.to_string())
        .unwrap_or_else(|| EFFORT_DEFAULT_TOKEN.to_string());

    [
        format!("provider   {}", view.provider.clone().unwrap_or_else(dash)),
        format!("model      {}", view.model_label()),
        format!("effort: {effort}"),
        format!("tokens     {tokens}"),
        format!("cache      {cache}"),
    ]
    .into_iter()
    // 压缩行**仅在有消耗时出现** —— 没压过的会话不必多一行全程为 `-` 的字段。
    .chain(view.compaction_tokens.map(|(input, output)| {
        format!(
            "compaction ↑{} ↓{}",
            crate::format::format_tokens_or_dash(input),
            crate::format::format_tokens_or_dash(output)
        )
    }))
    .chain([
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
    ])
    .collect::<Vec<_>>()
    .join("\n")
}

/// `/copy` 的正文：最后一条非空 assistant 文本。
///
/// 返回 `None` = 没有可复制的回复。
///
/// **文本去往终端剪贴板**（OSC 52，由 `run.rs` 发序列）—— 不走「写进对话区让用户
/// 手动选中」：`run.rs` 常开 `EnableMouseCapture`（ADR-0050 滚轮接管鼠标），
/// 终端原生拖选因此失效，「可手动选中」是一个走不通的指路。
pub fn copy_text(app: &App) -> Option<&str> {
    app.transcript.iter().rev().find_map(|line| match line {
        TranscriptLine::Assistant(s) if !s.trim().is_empty() => Some(s.as_str()),
        _ => None,
    })
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
        LocalAction::Keybind => keybind_text(),
        LocalAction::Copy => {
            // 借用在 `pending_clipboard` 写入前结束：先拿 owned 副本。
            let body = copy_text(app).map(str::to_owned);
            let Some(body) = body else {
                app.transcript.push(TranscriptLine::System(
                    "（/copy：还没有可复制的回复）".into(),
                ));
                app.follow = true;
                return;
            };
            let chars = body.chars().count();
            app.pending_clipboard = Some(body);
            format!("（/copy：已写入系统剪贴板，{chars} 字）")
        }
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
        // 主题**只有一份真值**（`App.theme`）—— 与 `/effort` 的「只读视图」不同：
        // 主题是纯 UI 状态，本地改完就是最终值，app 侧只是持久化镜像。落盘由
        // `events.rs` 单向同步（本函数只拿 `&mut App`，不做 IO）。
        LocalAction::Theme(kind) => {
            app.set_theme(kind);
            format!("theme: {}", kind.name())
        }
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
            ("/theme", Action::Prompt(PromptKind::PickTheme)),
            ("/theme nord", Action::Local(L::Theme(ThemeKind::Nord))),
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
            ("/clear", Action::Request(Request::ClearSession)),
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
    ///
    /// 变异：删掉 `parse_with_skills` 的 `clear` 分支 ⇒ `/clear` 落进未知命令的
    /// `Hint`，下面的 `缺 ClearSession` 断言变红。
    #[test]
    fn test_command_table_covers_every_request_variant() {
        let mut produced: Vec<Request> = [
            "hello",
            "/model gpt",
            "/effort high",
            "/logout",
            "/new",
            "/clear",
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
            has(|r| matches!(r, Request::ClearSession)),
            "缺 ClearSession"
        );
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

    /// `/theme` 的三种形态：无参开浮层、合法名（大小写不敏感 / `_` 归一化）落成
    /// 已解析的 [`ThemeKind`]、非法名只本地提示。
    ///
    /// 变异：把无参分支改成 `LocalAction::Hint`（不开浮层）⇒ 第一条断言变红；
    /// 把 `arg().parse::<ThemeKind>()` 换成 `Ok(ThemeKind::default())` ⇒ 后两条变红。
    #[test]
    fn test_theme_parse_three_forms() {
        // 无参 = 浮层（要实时预览，故不进通用 `Prompt` 路径也不直发）。
        assert_eq!(parse("/theme"), Action::Prompt(PromptKind::PickTheme));
        assert_eq!(parse("/theme   "), Action::Prompt(PromptKind::PickTheme));

        // 有参合法 ⇒ 已解析的主题值（名称归一化由 `ThemeKind::from_str` 决定）。
        assert_eq!(
            parse("/theme nord"),
            Action::Local(LocalAction::Theme(ThemeKind::Nord))
        );
        assert_eq!(
            parse("/theme NORD"),
            Action::Local(LocalAction::Theme(ThemeKind::Nord)),
            "大小写不敏感"
        );
        assert_eq!(
            parse("/theme gruvbox_dark"),
            Action::Local(LocalAction::Theme(ThemeKind::GruvboxDark)),
            "`_` 归一化为 `-`"
        );

        // 有参非法 ⇒ 本地提示且**列出全部合法名**（不改任何状态）。
        match parse("/theme nope") {
            Action::Local(LocalAction::Hint(text)) => {
                assert!(text.contains("nope"), "要回显原文：{text}");
                for kind in ThemeKind::ALL {
                    assert!(
                        text.contains(kind.name()),
                        "提示缺 `{}`：{text}",
                        kind.name()
                    );
                }
            }
            other => panic!("非法主题应本地提示，得到 {other:?}"),
        }
    }

    /// `/theme <name>` 的本地应用：**调 `App::set_theme`**（不是只写一行文本），
    /// 并把生效后的规范名写进 transcript。
    ///
    /// 变异：把臂里的 `app.set_theme(kind)` 删掉（只留文本）⇒ 第一条断言变红。
    #[test]
    fn test_apply_local_theme_sets_app_theme() {
        let mut a = app();
        assert_eq!(a.theme, ThemeKind::default(), "初始为主题默认值");

        apply_local(&mut a, LocalAction::Theme(ThemeKind::Nord));
        assert_eq!(a.theme, ThemeKind::Nord, "`/theme nord` 必须本地立即生效");
        let Some(TranscriptLine::System(text)) = a.transcript.last() else {
            panic!("应 push 一条 System：{:?}", a.transcript);
        };
        assert_eq!(text, "theme: nord", "回显用 kebab-case 规范名");
        assert!(a.follow, "本地动作要贴底");
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

    /// `/status` 的缓存行：三态必须分开报 —— 无读数写「provider 未上报」，
    /// 有读数给 `hit / miss（pct%）`，算不出百分比给 `-`。
    ///
    /// 变异：把 `None` 分支也拼成 `hit / miss`（或直接省略该行）⇒
    /// 第一条断言变红 —— 用户会以为「没测到」就是「0 命中」。
    #[test]
    fn status_text_reports_cache_readings_and_unreported() {
        let line_of = |v: &CodingView, key: &str| {
            status_text(v)
                .lines()
                .find(|l| l.starts_with(key))
                .map(|l| l.to_string())
        };

        // ① 无读数：明确写「provider 未上报」，不省略行。
        let view = CodingView::for_test();
        let cache = line_of(&view, "cache").expect("无读数也要有 cache 行");
        assert!(
            cache.contains("provider 未上报"),
            "无读数必须写明原因：{cache}"
        );

        // ② 有读数 + 可算百分比。
        let view = CodingView {
            cache_tokens: Some((900, 100)),
            cache_hit_pct: Some(90),
            ..CodingView::for_test()
        };
        assert_eq!(
            line_of(&view, "cache").unwrap(),
            "cache      900 / 100（90%）"
        );

        // ③ 有读数但算不出百分比。
        let view = CodingView {
            cache_tokens: Some((0, 0)),
            cache_hit_pct: None,
            ..CodingView::for_test()
        };
        assert_eq!(line_of(&view, "cache").unwrap(), "cache      0 / 0（-）");
    }

    /// `/status` 的 tokens 行：**无读数不省略**（写 `-`），有读数照旧。
    #[test]
    fn status_text_tokens_line_uses_dash_without_reading() {
        let line_of = |v: &CodingView| {
            status_text(v)
                .lines()
                .find(|l| l.starts_with("tokens"))
                .expect("`/status` 应有 tokens 行")
                .to_string()
        };

        let view = CodingView::for_test();
        assert_eq!(
            line_of(&view),
            "tokens     ↑- ↓-",
            "无读数写 - 而不是藏起来"
        );

        let view = CodingView {
            total_input_tokens: 1_200,
            total_output_tokens: 345,
            ..CodingView::for_test()
        };
        assert_eq!(line_of(&view), "tokens     ↑1.2k ↓345");
    }

    /// `/status` 的压缩行：**仅在有消耗时出现**（没压过的会话不多一行全 `-`）。
    #[test]
    fn status_text_compaction_line_only_when_compacted() {
        let has = |v: &CodingView| status_text(v).lines().any(|l| l.starts_with("compaction"));

        assert!(!has(&CodingView::for_test()), "没压过 ⇒ 无 compaction 行");

        let view = CodingView {
            compaction_tokens: Some((4_000, 600)),
            ..CodingView::for_test()
        };
        let text = status_text(&view);
        assert!(
            text.contains("compaction ↑4.0k ↓600"),
            "压过 ⇒ 报两个计数：{text}"
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
                "help", "keybind", "status", "skills", "copy", "quit", "thinking", "effort",
                "theme", "model", "login", "logout", "new", "clear", "resume", "compact", "export",
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

    /// `/help` 必须列出**键位摘要**，并指向 `/keybind` —— 滚动键此前没有任何界面
    /// 出口（用户只能试）。
    #[test]
    fn test_help_text_contains_key_bindings() {
        let text = help_text();
        assert!(text.contains("键位"), "help 必须有键位段");
        for key in [
            "PageUp",
            "PageDown",
            "Ctrl+↑",
            "Ctrl+Home",
            "Ctrl+End",
            "滚轮",
            "Home",
            "Ctrl+A",
            "Ctrl+V",
        ] {
            assert!(text.contains(key), "键位段必须列出 {key}");
        }
        assert!(
            text.contains("/keybind"),
            "摘要必须指向完整表，否则用户不知道还有正本：{text}"
        );
        // `⌘` 组合到不了应用（终端自己消费）⇒ 不得出现在键位表里。
        assert!(!text.contains('⌘'), "键位表不得列 ⌘ 组合（终端消费不到）");
    }

    // -----------------------------------------------------------------------
    // `/keybind`：完整键位表
    // -----------------------------------------------------------------------

    /// `/keybind` 必须把**每一条**键位都渲染出来，且按组分段。
    ///
    /// 变异：把 `keybind_text` 的循环写成只取第一组 ⇒ 本用例红（缺组标题）。
    #[test]
    fn test_keybind_text_covers_every_binding_and_group() {
        let text = keybind_text();
        assert!(text.starts_with("键位："), "应有标题：{text}");
        for group in KeyGroup::ALL {
            let in_group: Vec<&KeyBinding> = all_keybindings()
                .iter()
                .filter(|b| b.group == group)
                .collect();
            if in_group.is_empty() {
                continue;
            }
            assert!(
                text.contains(group.label()),
                "缺分组小标题「{}」：{text}",
                group.label()
            );
            for b in in_group {
                assert!(
                    text.contains(b.keys),
                    "漏了键「{}」（组 {}）",
                    b.keys,
                    group.label()
                );
                assert!(text.contains(b.action), "漏了行为说明「{}」", b.action);
            }
        }
        // 终端限制必须显式写出来 —— 否则用户会以为「本仓不支持 ⌘」是缺陷。
        assert!(text.contains("终端限制"), "必须说明终端侧限制");
    }

    /// 键位表本身的基本约束：键非空、行为非空、组内**无重名同义项**。
    #[test]
    fn test_keybindings_are_well_formed() {
        assert!(!all_keybindings().is_empty());
        for b in all_keybindings() {
            assert!(!b.keys.trim().is_empty(), "键不能为空");
            assert!(!b.action.trim().is_empty(), "{} 缺行为说明", b.keys);
        }
        // 组内同一个键组合不得出现两次（否则究竟哪条生效说不清）。
        for group in KeyGroup::ALL {
            let mut keys: Vec<&str> = all_keybindings()
                .iter()
                .filter(|b| b.group == group)
                .map(|b| b.keys)
                .collect();
            let n = keys.len();
            keys.sort_unstable();
            keys.dedup();
            assert_eq!(keys.len(), n, "{} 组内有重复键位", group.label());
        }
    }

    /// `/keybind` 是本地动作：解析落在本地，`apply_local` 把表写进 transcript，
    /// **不发请求**、不碰剪贴板。
    #[test]
    fn test_keybind_is_local_and_writes_into_transcript() {
        let mut a = app();
        assert_eq!(
            parse("/keybind"),
            Action::Local(LocalAction::Keybind),
            "解析必须落到本地动作"
        );
        let before = a.transcript.len();
        apply_local(&mut a, LocalAction::Keybind);
        assert_eq!(a.transcript.len(), before + 1, "应 push 一条 System 行");
        let Some(TranscriptLine::System(line)) = a.transcript.last() else {
            panic!("应写 System 行");
        };
        assert!(line.contains("键位："), "正文就是键位表：{line}");
        assert!(line.contains("Ctrl+A"), "完整表必须含全选键");
        assert!(a.pending_clipboard.is_none(), "不得碰剪贴板");
    }

    /// `Ctrl+C` 同时出现在「提交与退出」与「剪贴板」两组 —— 这是**有意的**：
    /// 同一个键两种语义，用户必须能在两处都查到。用注释钉住，避免以后被当成重复删掉。
    #[test]
    fn test_ctrl_c_is_listed_in_both_groups_on_purpose() {
        let groups: Vec<KeyGroup> = all_keybindings()
            .iter()
            .filter(|b| b.keys == "Ctrl+C")
            .map(|b| b.group)
            .collect();
        assert!(
            groups.contains(&KeyGroup::Submit),
            "Ctrl+C 必须列在提交与退出组"
        );
        assert!(
            groups.contains(&KeyGroup::Clipboard),
            "Ctrl+C 必须列在剪贴板组"
        );
    }

    #[test]
    fn test_copy_takes_last_assistant_text() {
        let mut a = app();
        a.transcript.push(TranscriptLine::User("q".into()));
        a.transcript.push(TranscriptLine::Assistant("first".into()));
        a.transcript
            .push(TranscriptLine::Assistant("second".into()));
        assert_eq!(copy_text(&a), Some("second"), "取最后一条非空 assistant");

        apply_local(&mut a, LocalAction::Copy);
        let Some(TranscriptLine::System(s)) = a.transcript.last() else {
            panic!("应 push System");
        };
        assert!(s.contains("剪贴板"), "确认行说明去向：{s}");
    }

    /// 没有回复时不得写剪贴板（`copy_text` 返回 `None` 是唯一真值）。
    #[test]
    fn test_copy_without_assistant_reply_says_so() {
        let a = app();
        assert_eq!(copy_text(&a), None);
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
        assert_eq!(copy_text(&a), None);
    }

    // -----------------------------------------------------------------------
    // `/copy` 走终端剪贴板（OSC 52）—— 不再依赖用户手动选中
    // -----------------------------------------------------------------------

    /// **回归测试（Bug：`/copy` 无法获取内容）**。
    ///
    /// 原实现把正文写进 `System` 行并提示"可手动选中"，但 `run.rs` 常开
    /// `EnableMouseCapture`（ADR-0050 为滚轮接管鼠标）⇒ 终端原生拖选失效
    /// ⇒ 提示所指的那条路**物理上走不通**，`/copy` 在任何终端上都不可能成功。
    ///
    /// 修复后：正文交给 `pending_clipboard`，由 `run.rs` 用 OSC 52 写进终端剪贴板。
    #[test]
    fn test_copy_hands_text_to_clipboard_instead_of_manual_selection() {
        let mut a = app();
        a.transcript
            .push(TranscriptLine::Assistant("正文一".into()));
        a.transcript
            .push(TranscriptLine::Assistant("正文二".into()));

        apply_local(&mut a, LocalAction::Copy);

        assert_eq!(
            a.pending_clipboard.as_deref(),
            Some("正文二"),
            "必须把最后一条回复交给剪贴板通道"
        );
        let Some(TranscriptLine::System(s)) = a.transcript.last() else {
            panic!("应 push System 确认行");
        };
        assert!(s.contains("剪贴板"), "确认行须说明已进剪贴板：{s}");
        assert!(
            !s.contains("手动选中"),
            "不得再让用户手动选中（鼠标捕获下走不通）：{s}"
        );
        assert!(
            !s.contains("正文二"),
            "正文不进 transcript（已在剪贴板）：{s}"
        );
    }

    /// 无回复可复制时**不得**改剪贴板。
    #[test]
    fn test_copy_without_reply_leaves_clipboard_untouched() {
        let mut a = app();
        apply_local(&mut a, LocalAction::Copy);
        assert!(a.pending_clipboard.is_none(), "无回复时不得写剪贴板");
        let Some(TranscriptLine::System(s)) = a.transcript.last() else {
            panic!("应 push System");
        };
        assert!(s.contains("还没有可复制的回复"), "{s}");
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

    /// context window 的缩写：整千/整百万不带小数，边界向下取整不越级。
    ///
    /// 变异：把 k 档改成 `{:.1}k` ⇒ `200k` / `262k` 断言变红；
    /// 把整百万判断去掉 ⇒ `1M` 变 `1.0M`。
    #[test]
    fn format_context_window_abbreviates_stably() {
        assert_eq!(format_context_window(200_000), "200k");
        assert_eq!(format_context_window(1_000_000), "1M");
        assert_eq!(format_context_window(262_144), "262k");
        assert_eq!(format_context_window(999_999), "999k", "不得进位成 1000k");
        assert_eq!(format_context_window(1_048_576), "1.0M");
        assert_eq!(format_context_window(1_500_000), "1.5M");
        assert_eq!(
            format_context_window(1_999_999),
            "1.9M",
            "M 档与 k 档同为截断，不得进位成 2.0M"
        );
        assert_eq!(format_context_window(65_536), "65k");
        assert_eq!(format_context_window(999), "999", "<1000 原样");
    }

    /// `/status` 的 context 行：declared ≠ effective 时并列两值并标 `(capped)`；
    /// 相等或 declared 缺失时保持单值（逐字节沿用既有快照行为）。
    ///
    /// 变异：去掉 `(capped)` 分支 ⇒ 第一条断言变红；把相等/缺失也走并列分支
    /// ⇒ 后两条断言变红。
    #[test]
    fn status_text_reports_capped_and_single_context_window() {
        let line = |v: &CodingView| {
            status_text(v)
                .lines()
                .find(|l| l.starts_with("context "))
                .expect("`/status` 应有 context 行")
                .to_string()
        };

        // declared ≠ effective ⇒ effective 在前、declared 在后 + `(capped)`。
        let view = CodingView {
            context_window: Some(200_000),
            context_window_declared: Some(1_000_000),
            ..CodingView::for_test()
        };
        let capped = line(&view);
        assert_eq!(capped, "context    200k / 1M (capped)");
        assert!(capped.contains("(capped)"), "{capped}");
        assert!(capped.contains("200k") && capped.contains("1M"), "{capped}");

        // 相等 ⇒ 单值，raw 数字（既有行为）。
        let view = CodingView {
            context_window: Some(65_536),
            context_window_declared: Some(65_536),
            ..CodingView::for_test()
        };
        let single = line(&view);
        assert_eq!(single, "context    65536");
        assert!(!single.contains("(capped)"), "{single}");

        // declared 缺失 ⇒ 退回单值（与既有快照行为一致）。
        let view = CodingView {
            context_window: Some(65_536),
            context_window_declared: None,
            ..CodingView::for_test()
        };
        let legacy = line(&view);
        assert_eq!(legacy, "context    65536");
        assert!(!legacy.contains("(capped)"), "{legacy}");
    }
}
