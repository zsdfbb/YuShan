use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use ys_core::{EffortLevel, Message};

use crate::{Envelope, SessionSummary};

/// UI → app 的能力请求（设计 §6）。
///
/// 在**回合边界**被 app 线程消费（`recv().await`）。命令的用户可见行为
/// （提示什么、问什么）归 UI；能力的实现归 app —— 漏实现是**编译错误**
/// （`match` 必须穷尽），不会静默漂移。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Request {
    /// agent 的输入（一条用户消息）。
    Prompt(Message),
    /// 切换当前模型。
    ///
    /// `provider` 是该模型条目的**归属**：`/model` 浮层选中时由 UI 带上、`/model
    /// provider/model` 限定名由 app 侧拆出；**手敲裸模型名时为 `None`** —— 那时
    /// 归属交 app 侧按模型目录反查（`ProviderRegistry::provider_for_model`）。
    ///
    /// 带上它是因为反查本身有歧义：两个 provider 的目录里可能有**逐字同名**的 id，
    /// 反查遍历的 `HashMap` 迭代顺序不确定，会静默把端点切到任一家。
    ///
    /// `#[serde(default)]`：后加字段的既定约定（先例 `Login.api_base`）—— 无此键的
    /// 旧 JSON（老日志 / 老 UI）必须仍能反序列化成 `None`。
    SetModel {
        model: String,
        #[serde(default)]
        provider: Option<String>,
    },
    /// 同步内联思考标签的显示开关。
    ///
    /// UI 侧 `App.thinking_visible` 是**运行期真值**，本请求只是把它**单向**
    /// 同步给 app 侧作持久化镜像（设计 §5.6）—— 不改变 UI 的本地即时生效语义。
    SetThinking { visible: bool },
    /// 设置思考档位（`None` = 清除设置，恢复 provider 默认）。
    ///
    /// 与 `SetThinking` 并列：前者是内联思考标签的**显示**开关，本请求是发给
    /// provider 的**思考深度**。值为 `None` 时由 app 侧删除已持久化的设置，
    /// 而不是把 `"default"` 之类的伪档位写进枚举。
    SetEffort { effort: Option<EffortLevel> },
    /// 切换配色主题。
    ///
    /// 载荷是**主题的 kebab-case 名**（如 `"nord"`），不是枚举：`ThemeKind` 定义在
    /// `ys-tui-coding`，而 `ys-protocol` 不认识 UI crate，也不该认识 —— 协议只承载
    /// 「UI 说用户选了哪个主题」这一层的最小足够表示。合法名表归 owner crate
    /// （`ThemeKind::from_str`），协议层**不解析、不校验**：
    /// 接线器只把它原样写进 `state.json`，解析回枚举发生在 `build_view` 时。
    /// 附带收益：新增主题不需要改协议。
    ///
    /// 与 `SetThinking` 并列：后者是「是否显示思考」的显示开关，本变体是配色选择。
    /// 两者都**不改变 UI 的本地即时生效语义** —— UI 先本地生效，再单向同步给 app 持久化。
    SetTheme { theme: String },
    /// 登录一个 provider 并持久化凭证。
    ///
    /// `api_base` 是**用户显式提供的** API base（`/login custom` 这类没有内置 base
    /// 的 provider 才需要）—— `None` 表示「按 app 侧的三级回退解析」（内置 base →
    /// `config.api_base`）。UI 只负责问，不问就 `None`，**绝不替 app 猜一个 URL**。
    ///
    /// `#[serde(default)]`：这是后加的字段，无此字段的旧 JSON（老日志 / 老 UI）
    /// 必须仍能反序列化成 `None`。
    Login {
        provider: String,
        api_key: String,
        #[serde(default)]
        api_base: Option<String>,
    },
    /// 登出当前 provider。
    Logout,
    /// 开新会话（换 `Session` + 空队列）。
    NewSession,
    /// 压缩上下文。
    ///
    /// **响应是两条 `Outbound::Output`**（先开始行、再收场行）—— 这是 `Request` 里唯一的
    /// 双输出变体。开始行必须等得到压缩那次 `await` 之前（`app_loop` 直接下发，不经
    /// `lines`）—— 摘要是一次独立模型调用，攒到最后发等于压缩全程冻屏。
    /// 压不了（无 model / 无事可压）时只有一条。
    Compact,
    /// 导出会话：`Some(path)` 为用户显式指定的路径，`None` 表示**由 app 侧
    /// 决定默认落点**（设计 §6「会话文件在 app 侧」）—— UI 不替 app 猜路径。
    Export { path: Option<PathBuf> },
    /// 列出既有会话（`/resume` 无参）。只读元数据，不加载消息体。
    ListSessions,
    /// 恢复既有会话。`target` 交给 app 侧 `sessions::resolve` 解析（id 或路径）。
    ResumeSession { target: String },
}

/// app → UI 的出站消息（设计 §6）。
///
/// `V` 是**产品视图**（coding agent 用 `CodingView`）：协议只提供通用壳，
/// 视图类型归各产品 TUI crate。故这里用标准 derive —— 自动为 `V` 加上
/// `Serialize` / `Deserialize` 约束，`ys-protocol` 不必认识任何具体视图。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Outbound<V> {
    /// 事件（已封壳，含 source / turn）。
    Event(Envelope),
    /// 视图快照。
    View(V),
    /// 命令输出 → transcript。
    Output(String),
    /// 会话列表（`Request::ListSessions` 的响应）。**空列表也要发** —— 让 UI
    /// 能提示「没有可恢复的会话」。
    SessionList(Vec<SessionSummary>),
    /// 会话已恢复 —— UI 见它即「清屏 + 回放」。三样必须**同时**生效，故装在同
    /// 一条消息里：分开传会出现「模型换了但屏上还是旧会话」的中间态。
    SessionResumed {
        summary: SessionSummary,
        /// 目标会话 meta 的 thinking 开关（照启动期 `--resume` 的初值语义）。
        thinking_visible: bool,
        /// 目标会话的完整历史 —— UI 侧负责转成 transcript 行。
        history: Vec<Message>,
    },
    /// 退出。
    Quit,
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;
    use serde_json::json;
    use ys_core::{ContentBlock, Role, ToolCallId};

    use crate::{Boundary, SessionSummary};

    /// 本地测试视图 —— 只用标准 derive 实现 serde，用来实例化
    /// `Outbound<TestView>`，验证泛型 derive 为 `V` 加上的
    /// `Serialize` / `Deserialize` 约束正确（`ys-protocol` 不必认识具体视图）。
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct TestView {
        n: u32,
    }

    fn text_message(role: Role, text: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// 通用断言：值 → JSON → 值，必须与原值相等；顺带记录 JSON 形态。
    fn assert_roundtrip<T>(value: &T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(*value, back, "roundtrip 不一致，json = {json}");
    }

    #[test]
    fn test_request_prompt_text_roundtrip() {
        assert_roundtrip(&Request::Prompt(text_message(Role::User, "hello")));
    }

    #[test]
    fn test_request_prompt_tooluse_roundtrip() {
        // Message 里含 ToolUse 块：走 ys_core 的 serde，arguments 是 Value
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "calling".into(),
                },
                ContentBlock::ToolUse {
                    id: ToolCallId("call_1".into()),
                    name: "bash".into(),
                    arguments: json!({ "cmd": "ls", "flag": true }),
                },
            ],
        };
        assert_roundtrip(&Request::Prompt(msg));
    }

    /// 带 provider 与不带都要能往返（两者都是合法输入：后者来自手敲裸名）。
    #[test]
    fn test_request_set_model_roundtrip() {
        assert_roundtrip(&Request::SetModel {
            model: "gpt-4o".into(),
            provider: Some("deepseek".into()),
        });
        assert_roundtrip(&Request::SetModel {
            model: "gpt-4o".into(),
            provider: None,
        });
    }

    #[test]
    fn test_request_set_thinking_roundtrip() {
        assert_roundtrip(&Request::SetThinking { visible: true });
        assert_roundtrip(&Request::SetThinking { visible: false });
    }

    /// `SetEffort` 的两种形态都要能过 serde：`Some` = 设定档位，
    /// `None` = 清除设置（恢复 provider 默认）。漏测 `None` 会让 app 侧
    /// 消费端在真实信道上炸。
    #[test]
    fn test_request_set_effort_roundtrip() {
        assert_roundtrip(&Request::SetEffort {
            effort: Some(EffortLevel::High),
        });
        assert_roundtrip(&Request::SetEffort { effort: None });
    }

    /// `SetTheme` 只承载 kebab-case 名（协议不解析、不校验），往返必须逐字节保真。
    ///
    /// 同时钉住**线格式**：变体名 `SetTheme` + 字段名 `theme` —— 跨进程 / 跨日志的
    /// 兼容全靠这两个名字，refactor 时被改名则旧 UI 与旧日志静默解析失败。
    ///
    /// 变异：把载荷类型改成 `String` 以外的包装 / 给字段加 `#[serde(rename)]` ⇒
    /// 下面那两处 `assert!(json.contains(..))` 变红。
    #[test]
    fn test_request_set_theme_roundtrip() {
        let req = Request::SetTheme {
            theme: "nord".into(),
        };
        assert_roundtrip(&req);

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"SetTheme\""), "变体名变化：{json}");
        assert!(json.contains("\"theme\":\"nord\""), "字段名变化：{json}");
    }

    /// `Login` 的两种 `api_base` 都要能过 serde：`Some` = 用户显式填的 URL
    /// （`custom`），`None` = 交给 app 侧回退。漏测任一种，app 侧消费端就可能
    /// 在真实信道上炸。
    #[test]
    fn test_request_login_roundtrip() {
        assert_roundtrip(&Request::Login {
            provider: "openai".into(),
            api_key: "sk-secret".into(),
            api_base: Some("https://api.example.com".into()),
        });
        assert_roundtrip(&Request::Login {
            provider: "deepseek".into(),
            api_key: "sk-secret".into(),
            api_base: None,
        });
    }

    /// **向后兼容**：老形态的 JSON（没有 `api_base` 键）必须仍能反序列化，
    /// 且落为 `None` —— 否则一次协议加字段就会让老 UI / 老日志在
    /// `serde_json::from_*` 处炸掉。
    #[test]
    fn test_request_login_without_api_base_key_deserializes_to_none() {
        let v = json!({ "Login": { "provider": "deepseek", "api_key": "sk-1" } });
        let req: Request = serde_json::from_value(v).unwrap();
        assert_eq!(
            req,
            Request::Login {
                provider: "deepseek".into(),
                api_key: "sk-1".into(),
                api_base: None,
            }
        );
    }

    /// **向后兼容**：老形态的 JSON（没有 `provider` 键）必须仍能反序列化，且落为
    /// `None` —— 否则一次协议加字段就会让老 UI / 老日志在 `serde_json::from_*` 处炸掉。
    ///
    /// 变异：删掉 `SetModel.provider` 上的 `#[serde(default)]` ⇒ 缺键即报错 ⇒ 本用例变红。
    #[test]
    fn test_request_set_model_without_provider_key_deserializes_to_none() {
        let v = json!({ "SetModel": { "model": "gpt-4o" } });
        let req: Request = serde_json::from_value(v).unwrap();
        assert_eq!(
            req,
            Request::SetModel {
                model: "gpt-4o".into(),
                provider: None,
            }
        );
    }

    #[test]
    fn test_request_unit_variants_roundtrip() {
        assert_roundtrip(&Request::Logout);
        assert_roundtrip(&Request::NewSession);
        assert_roundtrip(&Request::Compact);
    }

    /// `Export.path` 的两种形态都要能过 serde：`Some` = 用户显式路径，
    /// `None` = 默认落点由 app 侧决定。二者编码不同（`null` vs 字符串），
    /// 漏测任一种都会让 app 侧消费端在真实信道上炸。
    #[test]
    fn test_request_export_path_roundtrip() {
        assert_roundtrip(&Request::Export {
            path: Some(PathBuf::from("/tmp/out.jsonl")),
        });
        assert_roundtrip(&Request::Export { path: None });
    }

    #[test]
    fn test_outbound_event_roundtrip() {
        let env = Envelope::new(
            crate::Source::agent(),
            2,
            ys_event::AgentEvent::ModelTextDelta { text: "hi".into() },
        );
        assert_roundtrip(&Outbound::<TestView>::Event(env));
    }

    #[test]
    fn test_outbound_view_roundtrip() {
        assert_roundtrip(&Outbound::<TestView>::View(TestView { n: 42 }));
    }

    #[test]
    fn test_outbound_output_roundtrip() {
        assert_roundtrip(&Outbound::<TestView>::Output("done".into()));
    }

    #[test]
    fn test_outbound_quit_roundtrip() {
        assert_roundtrip(&Outbound::<TestView>::Quit);
    }

    /// 确定性构造的 `SessionSummary`（`mtime` 用固定 epoch 偏移而非 `now()`，
    /// 保证用例可复现）。覆盖 `PathBuf` / `SystemTime` 的 serde 往返。
    fn sample_summary() -> SessionSummary {
        SessionSummary {
            id: "2026-10-10-abc123".into(),
            path: PathBuf::from("/tmp/sessions/2026-10-10-abc123.jsonl"),
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_760_000_000),
            title: "修复 UTF-8 边界".into(),
        }
    }

    #[test]
    fn test_request_list_sessions_roundtrip() {
        assert_roundtrip(&Request::ListSessions);
    }

    #[test]
    fn test_request_resume_session_roundtrip() {
        assert_roundtrip(&Request::ResumeSession {
            target: "2026-10-10-abc123".into(),
        });
    }

    /// `SessionList` 非空：`SessionSummary` 的 `PathBuf` / `SystemTime` 字段
    /// 必须真的能经 JSON 往返（漏测则 app 侧回声列表时字段可能被静默丢失）。
    #[test]
    fn test_outbound_session_list_roundtrip() {
        assert_roundtrip(&Outbound::<TestView>::SessionList(vec![sample_summary()]));
    }

    /// `SessionResumed` 三样（summary / thinking_visible / history）必须同时往返。
    /// `history` 里放一条带 `ToolUse` 的 assistant 消息，覆盖 ys-core 的 serde
    /// （`arguments` 是 `Value`）。
    ///
    /// 变异：给 `thinking_visible` 加 `#[serde(skip)]`（令其不参与 roundtrip）
    /// ⇒ 本用例变红 —— 断言 thinking 开关随同一条消息一起送达，不会出现
    /// 「模型换了但屏上还是旧会话」的中间态。
    #[test]
    fn test_outbound_session_resumed_roundtrip() {
        let history = vec![
            text_message(Role::User, "你好"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "calling".into(),
                    },
                    ContentBlock::ToolUse {
                        id: ToolCallId("call_9".into()),
                        name: "bash".into(),
                        arguments: json!({ "cmd": "ls", "flag": false }),
                    },
                ],
            },
        ];
        assert_roundtrip(&Outbound::<TestView>::SessionResumed {
            summary: sample_summary(),
            thinking_visible: true,
            history,
        });
    }

    /// **向后兼容**：新变体是纯追加，既有变体的线格式不变 —— 旧形态 JSON
    /// （单元变体裸串 + 结构变体外部标签）必须仍能反序列化。
    #[test]
    fn test_legacy_request_variants_still_deserialize() {
        assert_eq!(
            serde_json::from_value::<Request>(json!("NewSession")).unwrap(),
            Request::NewSession
        );
        assert_eq!(
            serde_json::from_value::<Request>(json!({ "SetModel": { "model": "gpt-4o" } }))
                .unwrap(),
            Request::SetModel {
                model: "gpt-4o".into(),
                provider: None,
            }
        );
        assert_eq!(
            serde_json::from_value::<Request>(json!({ "Export": { "path": null } })).unwrap(),
            Request::Export { path: None }
        );
        // `SetEffort` 是加法式新增，线格式同样为外部标签 + 小写档位拼写。
        assert_eq!(
            serde_json::from_value::<Request>(json!({ "SetEffort": { "effort": "xhigh" } }))
                .unwrap(),
            Request::SetEffort {
                effort: Some(EffortLevel::XHigh),
            }
        );
        assert_eq!(
            serde_json::from_value::<Request>(json!({ "SetEffort": { "effort": null } })).unwrap(),
            Request::SetEffort { effort: None }
        );
    }

    /// 既有的 `Outbound` **线格式**（外部标签 + 单元变体裸串 + 新类型变体载荷）。
    ///
    /// roundtrip 只保证 ser/de **自洽**：对**对称改动**（给 `Outbound` 整体加
    /// `#[serde(tag = "kind")]`、重命名某字段）依旧通过，但真实信道上旧对端会解析
    /// 失败。此处钉死裸 JSON 形态，与 `Request` 侧的
    /// `test_legacy_request_variants_still_deserialize` 对等。用 Value 比较而非
    /// 字符串匹配，避免依赖 JSON 空白/转义细节。
    #[test]
    fn test_outbound_wire_format() {
        // 单元变体：裸字符串 `"Quit"`。
        assert_eq!(
            serde_json::to_value(Outbound::<TestView>::Quit).unwrap(),
            json!("Quit"),
            "Quit 线格式变化"
        );
        // 新类型变体：外部标签 + 载荷（`{"Output": "x"}`）。
        assert_eq!(
            serde_json::to_value(Outbound::<TestView>::Output("x".into())).unwrap(),
            json!({ "Output": "x" }),
            "Output 线格式变化"
        );
        // 新变体 `SessionResumed`：仅断言**顶层键名/结构**与设计 §3.2 一致
        // （summary / thinking_visible / history），否则 UI 侧读不到字段。
        // `summary.mtime` 是 `SystemTime`，其序列化形态属 serde 内部细节，
        // 钉死会很脆，故不深断言。
        let v = serde_json::to_value(Outbound::<TestView>::SessionResumed {
            summary: sample_summary(),
            thinking_visible: true,
            history: vec![text_message(Role::User, "hi")],
        })
        .unwrap();
        let outer = v.as_object().unwrap();
        assert_eq!(outer.len(), 1, "SessionResumed 外层应只含单个变体键：{v}");
        let inner = outer["SessionResumed"].as_object().unwrap();
        let mut keys: Vec<&str> = inner.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["history", "summary", "thinking_visible"],
            "SessionResumed 字段名与设计 §3.2 不一致：{v}"
        );
        assert_eq!(
            inner["thinking_visible"],
            json!(true),
            "thinking_visible 线格式变化：{v}"
        );
    }

    #[test]
    fn test_boundary_steer_roundtrip() {
        assert_roundtrip(&Boundary::Steer(text_message(Role::User, "steer me")));
    }

    #[test]
    fn test_boundary_abort_serde_shape() {
        // 实际形态（serde 默认外部标签 + 单元变体）：裸字符串 `"Abort"`。
        // 用 Value 比较而非字符串匹配，避免依赖 JSON 空白/转义细节。
        let v = serde_json::to_value(Boundary::Abort).unwrap();
        assert_eq!(v, json!("Abort"), "Abort 序列化形态变化：{v}");
        assert_eq!(
            serde_json::from_value::<Boundary>(v).unwrap(),
            Boundary::Abort
        );
    }
}
