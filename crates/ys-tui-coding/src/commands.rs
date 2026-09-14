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

use ys_core::{ContentBlock, Message, Role};
use ys_protocol::Request;

use crate::app::App;
use crate::transcript::TranscriptLine;
use crate::view::CodingView;

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
static COMMANDS: [CommandSpec; 11] = [
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
}

/// 解析一次提交的输入。
///
/// - 非 `/` 开头 → [`Action::Request`]`(`[`Request::Prompt`]`)`（普通消息）
/// - 未知 `/xxx` → 本地提示（[`Action::Local`]`(`[`LocalAction::Hint`]`)`）
/// - 见 [`all_commands`] 的逐条映射
pub fn parse(input: &str) -> Action {
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
        // **无参不直发 SetModel** —— 先开浮层选（设计 §6）
        "model" => {
            if args.is_empty() {
                Action::Prompt(PromptKind::PickModel)
            } else {
                Action::Request(Request::SetModel { model: arg() })
            }
        }
        "login" => Action::Prompt(PromptKind::Login),
        "logout" => Action::Request(Request::Logout),
        "new" => Action::Request(Request::NewSession),
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
        _ => Action::Local(LocalAction::Hint(unknown_hint(text))),
    }
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

/// `/help` 的正文 —— 与补全浮层同源（同一张 [`all_commands`]）。
pub fn help_text() -> String {
    let mut out = String::from("可用命令：\n");
    for spec in all_commands() {
        out.push_str("  ");
        out.push_str(&spec_line(spec));
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
    let ctx = view
        .context_window
        .map(|c| c.to_string())
        .unwrap_or_else(dash);

    [
        format!("provider   {}", view.provider.clone().unwrap_or_else(dash)),
        format!("model      {}", view.model_label()),
        format!("tokens     {tokens}"),
        format!("turns      {}", view.turn_count),
        format!("messages   {}", view.message_count),
        format!("duration   {}", view.session_duration_str()),
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
            None => help_text(),
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
            ("/copy", Action::Local(L::Copy)),
            ("/quit", Action::Quit),
            ("/thinking", Action::Local(L::Thinking(None))),
            ("/thinking on", Action::Local(L::Thinking(Some(true)))),
            ("/thinking off", Action::Local(L::Thinking(Some(false)))),
            ("/model", Action::Prompt(PromptKind::PickModel)),
            (
                "/model gpt-4o",
                Action::Request(Request::SetModel {
                    model: "gpt-4o".into(),
                }),
            ),
            ("/login", Action::Prompt(PromptKind::Login)),
            ("/logout", Action::Request(Request::Logout)),
            ("/new", Action::Request(Request::NewSession)),
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
    /// `matches!` 已知的 7 个变体，新增变体不会让它变红。真正与 `Request`
    /// 对齐的闭环在 app 侧：app 循环对 `Request` 的穷尽 `match` 是**编译错误**，
    /// 漏实现编不过。本测试只保证这张表本身完整（每条命令都真能产出请求，
    /// 而不是解析成 `None` / `Hint` 被悄悄吞掉）。
    #[test]
    fn test_command_table_covers_every_request_variant() {
        let mut produced: Vec<Request> = [
            "hello",
            "/model gpt",
            "/logout",
            "/new",
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
        let fake = crate::prompter::FakePrompter::ok(["deepseek", "sk-1", "deepseek-chat"]);
        for input in ["/login", "/model"] {
            match parse(input) {
                Action::Prompt(kind) => produced.push(
                    crate::prompter::resolve_prompt(kind, &fake, &view)
                        .unwrap_or_else(|e| panic!("`{input}` 应能产出 Request：{e}")),
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
        assert!(has(|r| matches!(r, Request::Login { .. })), "缺 Login");
        assert!(has(|r| matches!(r, Request::Logout)), "缺 Logout");
        assert!(has(|r| matches!(r, Request::NewSession)), "缺 NewSession");
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
                "help", "status", "copy", "quit", "thinking", "model", "login", "logout", "new",
                "compact", "export",
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
}
